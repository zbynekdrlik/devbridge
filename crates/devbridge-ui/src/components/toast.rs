use leptos::prelude::*;

#[derive(Clone, Debug)]
pub struct Toast {
    pub id: usize,
    pub message: String,
    pub level: ToastLevel,
}

#[derive(Clone, Debug)]
pub enum ToastLevel {
    Info,
    Success,
    Warning,
    Error,
}

impl ToastLevel {
    pub fn border_color(&self) -> &'static str {
        match self {
            ToastLevel::Info => "border-left: 3px solid var(--primary)",
            ToastLevel::Success => "border-left: 3px solid var(--success)",
            ToastLevel::Warning => "border-left: 3px solid var(--warning)",
            ToastLevel::Error => "border-left: 3px solid var(--danger)",
        }
    }
}

#[component]
pub fn ToastContainer() -> impl IntoView {
    let (toasts, _set_toasts) = signal::<Vec<Toast>>(vec![]);

    view! {
        <div class="toast-container">
            {move || {
                toasts.get().iter().map(|toast| {
                    let style = toast.level.border_color().to_string();
                    let msg = toast.message.clone();
                    view! {
                        <div class="toast" style=style>
                            {msg}
                        </div>
                    }
                }).collect_view()
            }}
        </div>
    }
}
