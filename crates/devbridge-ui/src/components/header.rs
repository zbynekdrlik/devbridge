use leptos::prelude::*;

#[component]
pub fn PageHeader(#[prop(into)] title: String) -> impl IntoView {
    view! {
        <div class="header">
            <h2>{title}</h2>
        </div>
    }
}
