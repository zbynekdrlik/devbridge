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
    #[allow(dead_code)]
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

/// Shared toast state — provide this at the app root and use `push_toast` to add toasts.
#[derive(Clone)]
pub struct ToastState {
    pub toasts: ReadSignal<Vec<Toast>>,
    set_toasts: WriteSignal<Vec<Toast>>,
    next_id: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl ToastState {
    pub fn new() -> Self {
        let (toasts, set_toasts) = signal::<Vec<Toast>>(vec![]);
        Self {
            toasts,
            set_toasts,
            next_id: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }

    pub fn push(&self, message: String, level: ToastLevel) {
        let id = self
            .next_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let toast = Toast {
            id,
            message,
            level,
        };
        self.set_toasts.update(|t| t.push(toast));

        // Auto-dismiss after 5 seconds
        let set_toasts = self.set_toasts;
        leptos::task::spawn_local(async move {
            gloo_timers::future::TimeoutFuture::new(5_000).await;
            set_toasts.update(|t| t.retain(|toast| toast.id != id));
        });
    }
}

#[component]
pub fn ToastContainer() -> impl IntoView {
    let state = expect_context::<ToastState>();

    view! {
        <div class="toast-container" style="position: fixed; top: 1rem; right: 1rem; z-index: 1000; display: flex; flex-direction: column; gap: 0.5rem; max-width: 400px">
            {move || {
                state.toasts.get().iter().map(|toast| {
                    let style = format!(
                        "{}; padding: 0.75rem 1rem; background: var(--bg-primary); border-radius: 4px; box-shadow: 0 2px 8px rgba(0,0,0,0.3)",
                        toast.level.border_color()
                    );
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
