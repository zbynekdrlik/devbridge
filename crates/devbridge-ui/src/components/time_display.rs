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

/// Displays only the time portion (e.g., "14:32") in local timezone.
#[component]
pub fn TimeOnly(
    /// RFC3339 datetime string
    datetime: String,
) -> impl IntoView {
    let formatted = format_local_time(&datetime);
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

/// Format an RFC3339 string to local time only (HH:MM).
fn format_local_time(rfc3339: &str) -> String {
    let date = js_sys::Date::new(&wasm_bindgen::JsValue::from_str(rfc3339));
    if date.to_string() == "Invalid Date" {
        return rfc3339.to_string();
    }
    let hours = date.get_hours();
    let minutes = date.get_minutes();
    format!("{hours:02}:{minutes:02}")
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
