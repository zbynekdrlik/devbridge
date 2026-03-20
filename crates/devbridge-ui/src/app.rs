use leptos::prelude::*;
use leptos_router::components::{Route, Router, Routes};
use leptos_router::path;

use crate::components::sidebar::Sidebar;
use crate::components::toast::ToastContainer;
use crate::pages::config::ConfigPage;
use crate::pages::dashboard::DashboardPage;
use crate::pages::jobs::JobsPage;
use crate::pages::logs::LogsPage;
use crate::pages::printers::PrintersPage;

#[component]
pub fn App() -> impl IntoView {
    view! {
        <Router>
            <div class="app-layout">
                <Sidebar />
                <main class="main-content">
                    <Routes fallback=|| view! { <p>"Page not found."</p> }>
                        <Route path=path!("/") view=DashboardPage />
                        <Route path=path!("/jobs") view=JobsPage />
                        <Route path=path!("/printers") view=PrintersPage />
                        <Route path=path!("/config") view=ConfigPage />
                        <Route path=path!("/logs") view=LogsPage />
                    </Routes>
                </main>
            </div>
            <ToastContainer />
        </Router>
    }
}
