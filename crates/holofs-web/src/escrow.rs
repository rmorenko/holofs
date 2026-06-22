//! `GET /escrow` — Leptos SSR page with split + recover forms.
//!
//! POST endpoints (`/escrow/split`, `/escrow/recover`) and binary download
//! (`/escrow/download/<id>_<idx>.holoshare`) live in
//! [`crate::handlers`] as plain axum handlers — they deal with multipart
//! parsing and binary I/O, not HTML.

use leptos::prelude::*;

use crate::t;
use crate::ui::Topbar;

/// `GET /escrow` — static UI: introduces escrow, hosts split + recover forms.
#[component]
pub fn EscrowPage() -> impl IntoView {
    view! {
        <Topbar active="escrow"/>

        <main class="container">
            <h2>{t!("escrow.title")}</h2>
            <p>
                "Split any file into "<code>"n"</code>" shares so that any "<code>"k"</code>
                " of them can reconstruct it (Shamir-style secret sharing on RLNC). "
                "Distribute the shares across trusted people or places — anything "
                "fewer than "<code>"k"</code>" shares leaks no information about the file "
                "(information-theoretic, not just hard-to-break)."
            </p>
            <p class="mut">
                "Shares live in the gateway memory only. After the gateway restarts "
                "they vanish, so download them immediately."
            </p>

            <section class="escrow-section">
                <h3>{t!("escrow.split_h")}</h3>
                <form
                    method="POST"
                    action="/escrow/split"
                    enctype="multipart/form-data"
                    class="escrow-form"
                >
                    <label>
                        <span class="lbl">"file"</span>
                        <input type="file" name="file" required=true/>
                    </label>
                    <label>
                        <span class="lbl">"k (threshold)"</span>
                        <input type="number" name="k" value="3" min="1" max="64" required=true/>
                    </label>
                    <label>
                        <span class="lbl">"n (total shares)"</span>
                        <input type="number" name="n" value="5" min="1" max="64" required=true/>
                    </label>
                    <button type="submit">{t!("escrow.btn.split")}</button>
                </form>
            </section>

            <section class="escrow-section">
                <h3>{t!("escrow.recover_h")}</h3>
                <form
                    method="POST"
                    action="/escrow/recover"
                    enctype="multipart/form-data"
                    class="escrow-form"
                >
                    <label>
                        <span class="lbl">".holoshare files (≥ k)"</span>
                        <input type="file" name="shares" multiple=true required=true/>
                    </label>
                    <button type="submit">{t!("escrow.btn.recover")}</button>
                </form>
            </section>
        </main>
    }
}
