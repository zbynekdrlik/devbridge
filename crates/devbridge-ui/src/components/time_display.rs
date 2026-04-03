use leptos::prelude::*;

/// Displays an RFC3339 datetime string formatted in the browser's local timezone
/// using the browser's locale conventions.
#[component]
pub fn TimeDisplay(
    /// RFC3339 datetime string (e.g., "2026-03-29T14:30:00Z")
    datetime: String,
) -> impl IntoView {
    let formatted = format_local_datetime(&datetime);
    view! {
        <time datetime=datetime.clone() title=datetime>
            {formatted}
        </time>
    }
}

/// Format an RFC3339 string to local date+time using JS Date API.
fn format_local_datetime(rfc3339: &str) -> String {
    let date = js_sys::Date::new(&wasm_bindgen::JsValue::from_str(rfc3339));
    if date.to_string() == "Invalid Date" {
        return rfc3339.to_string();
    }
    // toLocaleString() uses browser locale automatically
    date.to_locale_string("default", &js_sys::Object::new())
        .as_string()
        .unwrap_or_else(|| rfc3339.to_string())
}

/// Displays only the time portion with seconds (e.g., "14:32:07") in local timezone.
#[component]
pub fn TimeWithSeconds(
    /// RFC3339 datetime string
    datetime: String,
) -> impl IntoView {
    let formatted = format_local_time_with_seconds(&datetime);
    view! {
        <time datetime=datetime.clone() title=datetime>
            {formatted}
        </time>
    }
}

/// Format an RFC3339 string to local time with seconds (HH:MM:SS).
fn format_local_time_with_seconds(rfc3339: &str) -> String {
    let date = js_sys::Date::new(&wasm_bindgen::JsValue::from_str(rfc3339));
    if date.to_string() == "Invalid Date" {
        return rfc3339.to_string();
    }
    let hours = date.get_hours();
    let minutes = date.get_minutes();
    let seconds = date.get_seconds();
    format!("{hours:02}:{minutes:02}:{seconds:02}")
}

/// Displays current time, updating every second.
#[component]
pub fn CurrentTime() -> impl IntoView {
    let (time, set_time) = signal(format_now());
    leptos::task::spawn_local(async move {
        loop {
            gloo_timers::future::TimeoutFuture::new(1_000).await;
            set_time.set(format_now());
        }
    });
    view! {
        <span style="font-family: monospace; font-size: 0.9em; color: var(--text-muted)">
            {move || time.get()}
        </span>
    }
}

fn format_now() -> String {
    let now = js_sys::Date::new_0();
    let h = now.get_hours();
    let m = now.get_minutes();
    let s = now.get_seconds();
    format!("{h:02}:{m:02}:{s:02}")
}

/// Format "X ago" from RFC3339 timestamp.
pub fn format_time_ago(rfc3339: &str) -> String {
    let date = js_sys::Date::new(&wasm_bindgen::JsValue::from_str(rfc3339));
    if date.to_string() == "Invalid Date" {
        return "".to_string();
    }
    let now = js_sys::Date::new_0();
    let diff_ms = now.get_time() - date.get_time();
    let diff_secs = (diff_ms / 1000.0) as i64;

    if diff_secs < 0 {
        "just now".to_string()
    } else if diff_secs < 60 {
        format!("{diff_secs}s ago")
    } else if diff_secs < 3600 {
        format!("{}m ago", diff_secs / 60)
    } else if diff_secs < 86400 {
        format!("{}h ago", diff_secs / 3600)
    } else {
        format!("{}d ago", diff_secs / 86400)
    }
}

/// Returns a date group label for grouping jobs ("Today", "Yesterday", or locale date).
pub fn date_group_label(rfc3339: &str) -> String {
    let date = js_sys::Date::new(&wasm_bindgen::JsValue::from_str(rfc3339));
    if date.to_string() == "Invalid Date" {
        return "Unknown".to_string();
    }

    let now = js_sys::Date::new_0();
    let today_start = js_sys::Date::new_with_year_month_day(
        now.get_full_year(),
        now.get_month() as i32,
        now.get_date() as i32,
    );
    let yesterday_start = js_sys::Date::new_with_year_month_day(
        now.get_full_year(),
        now.get_month() as i32,
        now.get_date() as i32 - 1,
    );

    let ts = date.get_time();
    if ts >= today_start.get_time() {
        "Today".to_string()
    } else if ts >= yesterday_start.get_time() {
        "Yesterday".to_string()
    } else {
        date.to_locale_date_string("default", &js_sys::Object::new())
            .as_string()
            .unwrap_or_else(|| "Unknown".to_string())
    }
}
