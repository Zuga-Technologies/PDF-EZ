import { invoke } from "@tauri-apps/api/core";

export interface PageDims {
  width: number; // PDF points (1/72 in)
  height: number;
}

export interface DocInfo {
  id: number;
  page_count: number;
  pages: PageDims[];
  title: string | null;
  path: string;
}

export interface RenderedPage {
  index: number;
  width_px: number;
  height_px: number;
  data_url: string;
}

export interface FieldInfo {
  page: number;
  index: number;
  kind: string;
  name: string;
  value: string;
  checked: boolean;
  left: number;
  bottom: number;
  right: number;
  top: number;
}

export interface SearchHit {
  hit: number;
  page: number;
  left: number;
  bottom: number;
  right: number;
  top: number;
}

export interface AnnotInfo {
  page: number;
  index: number;
  kind: string;
  left: number;
  bottom: number;
  right: number;
  top: number;
  contents: string | null;
}

export interface Rgba {
  r: number;
  g: number;
  b: number;
  a: number;
}

export interface LicenseStatus {
  licensed: boolean;
  email: string | null;
  dev_build: boolean;
  reason: string | null;
}

export const api = {
  openDocument: (path: string, password?: string) =>
    invoke<DocInfo>("open_document", { path, password: password ?? null }),
  closeDocument: (id: number) => invoke<void>("close_document", { id }),
  renderPage: (id: number, index: number, targetWidth: number, jpeg: boolean) =>
    invoke<RenderedPage>("render_page", { id, index, targetWidth, jpeg }),
  getPageText: (id: number, index: number) =>
    invoke<string>("get_page_text", { id, index }),
  rotatePages: (id: number, indices: number[], quarterTurns: number) =>
    invoke<DocInfo>("rotate_pages", { id, indices, quarterTurns }),
  deletePages: (id: number, indices: number[]) =>
    invoke<DocInfo>("delete_pages", { id, indices }),
  reorderPages: (id: number, order: number[]) =>
    invoke<DocInfo>("reorder_pages", { id, order }),
  saveDocument: (id: number, path?: string) =>
    invoke<DocInfo>("save_document", { id, path: path ?? null }),
  mergeAppend: (id: number, paths: string[]) =>
    invoke<DocInfo>("merge_append", { id, paths }),
  listFormFields: (id: number, page: number) =>
    invoke<FieldInfo[]>("list_form_fields", { id, page }),
  setFormField: (id: number, page: number, index: number, value: string) =>
    invoke<FieldInfo[]>("set_form_field", { id, page, index, value }),
  searchDocument: (id: number, query: string, matchCase: boolean) =>
    invoke<SearchHit[]>("search_document", { id, query, matchCase }),
  compressDocument: (id: number) =>
    invoke<{ info: DocInfo; bytes_before: number; bytes_after: number }>("compress_document", {
      id,
    }),
  listAnnotations: (id: number, page: number) =>
    invoke<AnnotInfo[]>("list_annotations", { id, page }),
  addHighlight: (
    id: number,
    page: number,
    rect: { left: number; bottom: number; right: number; top: number },
    color: Rgba,
  ) => invoke<AnnotInfo[]>("add_highlight", { id, page, ...rect, color }),
  addNote: (id: number, page: number, x: number, y: number, contents: string) =>
    invoke<AnnotInfo[]>("add_note", { id, page, x, y, contents }),
  addInk: (id: number, page: number, strokes: [number, number][][], color: Rgba, width: number) =>
    invoke<AnnotInfo[]>("add_ink", { id, page, strokes, color, width }),
  deleteAnnotation: (id: number, page: number, index: number) =>
    invoke<AnnotInfo[]>("delete_annotation", { id, page, index }),
  checkLicense: () => invoke<LicenseStatus>("check_license"),
  activateLicenseFile: (leasePath: string) =>
    invoke<LicenseStatus>("activate_license_file", { leasePath }),
};
