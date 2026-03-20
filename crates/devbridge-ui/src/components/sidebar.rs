use leptos::prelude::*;

#[component]
pub fn Sidebar() -> impl IntoView {
    view! {
        <nav class="sidebar">
            <h1>"DevBridge"</h1>
            <a href="/">"Dashboard"</a>
            <a href="/jobs">"Jobs"</a>
            <a href="/printers">"Printers"</a>
            <a href="/config">"Config"</a>
            <a href="/logs">"Logs"</a>
        </nav>
    }
}
