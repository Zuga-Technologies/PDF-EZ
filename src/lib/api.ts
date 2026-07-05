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
  checkLicense: () => invoke<LicenseStatus>("check_license"),
  activateLicenseFile: (leasePath: string) =>
    invoke<LicenseStatus>("activate_license_file", { leasePath }),
};
