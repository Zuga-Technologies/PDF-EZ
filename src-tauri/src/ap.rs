//! Appearance-stream fixer (lopdf). PDFium never synthesizes appearance
//! streams for markup annotations, and neither do Chrome/Edge (same engine) —
//! so an AP-less /Highlight is invisible everywhere except Acrobat. This
//! module post-processes PDF bytes: every highlight without an /AP gets a
//! proper form XObject (multiply-blended fill, so text shows through), and
//! AcroForm gets /NeedAppearances so form values render in other viewers.
//!
//! Runs best-effort at open and save; encrypted bytes are passed through
//! untouched (they get fixed on the decrypted save path).

use lopdf::{dictionary, Document, Object, ObjectId, Stream};

/// Best-effort: on any parse/processing failure the original bytes win.
pub fn fix_appearance_streams(bytes: Vec<u8>) -> Vec<u8> {
    match try_fix(&bytes) {
        Ok(Some(fixed)) => fixed,
        _ => bytes,
    }
}

fn try_fix(bytes: &[u8]) -> lopdf::Result<Option<Vec<u8>>> {
    let mut doc = Document::load_mem(bytes)?;
    if doc.is_encrypted() {
        return Ok(None);
    }

    let mut changed = set_need_appearances(&mut doc);

    // Annotations may be referenced objects OR inline dictionaries in the
    // page's /Annots array (pdfium writes inline). Track both shapes.
    enum Slot {
        ByRef(ObjectId),
        /// (container holding the /Annots array, position in the array);
        /// container is the page itself or the array object /Annots points at.
        Inline(Container, usize),
    }
    enum Container {
        Page(ObjectId),
        ArrayObj(ObjectId),
    }

    let mut jobs: Vec<(Slot, Vec<[f32; 4]>, (f32, f32, f32), [f32; 4])> = Vec::new();
    let page_ids: Vec<ObjectId> = doc.get_pages().values().copied().collect();
    for page_id in page_ids {
        let (elements, container): (Vec<Object>, Container) = {
            let Ok(page) = doc.get_dictionary(page_id) else { continue };
            match page.get(b"Annots") {
                Ok(Object::Array(a)) => (a.clone(), Container::Page(page_id)),
                Ok(Object::Reference(r)) => match doc.get_object(*r) {
                    Ok(Object::Array(a)) => (a.clone(), Container::ArrayObj(*r)),
                    _ => continue,
                },
                _ => continue,
            }
        };
        for (pos, element) in elements.iter().enumerate() {
            let (annot, slot) = match element {
                Object::Reference(r) => match doc.get_dictionary(*r) {
                    Ok(d) => (d, Slot::ByRef(*r)),
                    Err(_) => continue,
                },
                Object::Dictionary(d) => (
                    d,
                    Slot::Inline(
                        match container {
                            Container::Page(p) => Container::Page(p),
                            Container::ArrayObj(a) => Container::ArrayObj(a),
                        },
                        pos,
                    ),
                ),
                _ => continue,
            };
            let is_highlight = annot
                .get(b"Subtype")
                .and_then(|o| o.as_name())
                .map(|n| n == b"Highlight")
                .unwrap_or(false);
            // Replace missing APs AND pdfium's auto-generated ones — the
            // latter render wrong (misplaced on rotated pages, invisible in
            // some viewers). Hand-authored APs (e.g. Acrobat's) are kept.
            let pdfium_generated = matches!(
                annot.get(b"PDFIUM_HasGeneratedAP").and_then(|o| o.as_bool()),
                Ok(true)
            );
            if !is_highlight || (annot.has(b"AP") && !pdfium_generated) {
                continue;
            }
            let Some(rect) = read_rect(annot.get(b"Rect").ok()) else { continue };
            let quads = read_quads(annot.get(b"QuadPoints").ok()).unwrap_or_else(|| vec![rect]);
            // pdfium writes markup color to /IC; Acrobat uses /C.
            let color = read_color(annot.get(b"C").ok())
                .or_else(|| read_color(annot.get(b"IC").ok()))
                .unwrap_or((1.0, 0.86, 0.24));
            jobs.push((slot, quads, color, rect));
        }
    }

    for (slot, quads, color, rect) in jobs {
        let ap_id = build_highlight_ap(&mut doc, &quads, color, rect);
        let ap_value = Object::Dictionary(dictionary! { "N" => Object::Reference(ap_id) });
        match slot {
            Slot::ByRef(annot_id) => {
                if let Ok(annot) = doc.get_dictionary_mut(annot_id) {
                    annot.set(b"AP".to_vec(), ap_value);
                    annot.remove(b"PDFIUM_HasGeneratedAP");
                    changed = true;
                }
            }
            Slot::Inline(container, pos) => {
                let array = match container {
                    Container::Page(page_id) => doc
                        .get_dictionary_mut(page_id)
                        .ok()
                        .and_then(|p| p.get_mut(b"Annots").ok())
                        .and_then(|o| o.as_array_mut().ok()),
                    Container::ArrayObj(array_id) => {
                        doc.get_object_mut(array_id).ok().and_then(|o| o.as_array_mut().ok())
                    }
                };
                if let Some(array) = array {
                    if let Some(Object::Dictionary(d)) = array.get_mut(pos) {
                        d.set(b"AP".to_vec(), ap_value);
                        d.remove(b"PDFIUM_HasGeneratedAP");
                        changed = true;
                    }
                }
            }
        }
    }

    if !changed {
        return Ok(None);
    }
    let mut out = Vec::new();
    doc.save_to(&mut out)?;
    Ok(Some(out))
}

/// /NeedAppearances lets other viewers regenerate form-field appearances,
/// so values we set through PDFium show up everywhere.
fn set_need_appearances(doc: &mut Document) -> bool {
    let Ok(root) = doc.trailer.get(b"Root").and_then(Object::as_reference) else { return false };
    let acroform_id = match doc.get_dictionary(root).and_then(|r| r.get(b"AcroForm")) {
        Ok(Object::Reference(r)) => Some(*r),
        _ => None,
    };
    let Some(acroform_id) = acroform_id else { return false };
    if let Ok(form) = doc.get_dictionary_mut(acroform_id) {
        if !matches!(form.get(b"NeedAppearances").and_then(|o| o.as_bool()), Ok(true)) {
            form.set(b"NeedAppearances".to_vec(), true);
            return true;
        }
    }
    false
}

fn build_highlight_ap(
    doc: &mut Document,
    quads: &[[f32; 4]],
    (r, g, b): (f32, f32, f32),
    rect: [f32; 4],
) -> ObjectId {
    let mut content = format!("/G0 gs {r:.3} {g:.3} {b:.3} rg\n");
    for [l, bt, rt, tp] in quads {
        content.push_str(&format!("{l:.2} {bt:.2} {:.2} {:.2} re f\n", rt - l, tp - bt));
    }

    let resources = dictionary! {
        "ExtGState" => dictionary! {
            "G0" => dictionary! { "BM" => Object::Name(b"Multiply".to_vec()) },
        },
    };
    let dict = dictionary! {
        "Type" => Object::Name(b"XObject".to_vec()),
        "Subtype" => Object::Name(b"Form".to_vec()),
        "BBox" => Object::Array(rect.iter().map(|v| Object::Real(*v)).collect()),
        "Group" => dictionary! { "S" => Object::Name(b"Transparency".to_vec()) },
        "Resources" => resources,
    };
    doc.add_object(Stream::new(dict, content.into_bytes()))
}

fn read_rect(obj: Option<&Object>) -> Option<[f32; 4]> {
    let arr = as_array(obj)?;
    if arr.len() != 4 {
        return None;
    }
    let v: Vec<f32> = arr.iter().filter_map(number).collect();
    if v.len() != 4 {
        return None;
    }
    Some([v[0].min(v[2]), v[1].min(v[3]), v[0].max(v[2]), v[1].max(v[3])])
}

/// QuadPoints: groups of 8 numbers (four x,y corners per quad).
fn read_quads(obj: Option<&Object>) -> Option<Vec<[f32; 4]>> {
    let arr = as_array(obj)?;
    let v: Vec<f32> = arr.iter().filter_map(number).collect();
    if v.is_empty() || v.len() % 8 != 0 {
        return None;
    }
    Some(
        v.chunks(8)
            .map(|q| {
                let xs = [q[0], q[2], q[4], q[6]];
                let ys = [q[1], q[3], q[5], q[7]];
                [
                    xs.iter().copied().fold(f32::MAX, f32::min),
                    ys.iter().copied().fold(f32::MAX, f32::min),
                    xs.iter().copied().fold(f32::MIN, f32::max),
                    ys.iter().copied().fold(f32::MIN, f32::max),
                ]
            })
            .collect(),
    )
}

fn read_color(obj: Option<&Object>) -> Option<(f32, f32, f32)> {
    let arr = as_array(obj)?;
    let v: Vec<f32> = arr.iter().filter_map(number).collect();
    (v.len() == 3).then(|| (v[0], v[1], v[2]))
}

fn as_array(obj: Option<&Object>) -> Option<&Vec<Object>> {
    match obj {
        Some(Object::Array(a)) => Some(a),
        _ => None,
    }
}

fn number(o: &Object) -> Option<f32> {
    match o {
        Object::Integer(i) => Some(*i as f32),
        Object::Real(r) => Some(*r),
        _ => None,
    }
}
