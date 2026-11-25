//! Global-scope helpers that work both on the main thread (`Window`) and in a
//! dedicated worker (`DedicatedWorkerGlobalScope`).

use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;

fn global() -> JsValue {
    js_sys::global().into()
}

fn function(name: &str) -> Option<js_sys::Function> {
    js_sys::Reflect::get(&global(), &JsValue::from_str(name))
        .ok()?
        .dyn_into::<js_sys::Function>()
        .ok()
}

fn call(name: &str, args: &[&JsValue]) -> Result<JsValue, JsValue> {
    let f = function(name).ok_or_else(|| JsValue::from_str(&format!("{name} unavailable")))?;
    let array = js_sys::Array::new();
    for a in args {
        array.push(a);
    }
    f.apply(&global(), &array)
}

/// Milliseconds on the monotonic, sub-millisecond clock (`performance.now()`,
/// available on the main thread and in workers), for measuring intervals.
pub fn performance_now() -> f64 {
    let now = || {
        let performance = js_sys::Reflect::get(&global(), &"performance".into()).ok()?;
        js_sys::Reflect::get(&performance, &"now".into())
            .ok()?
            .dyn_into::<js_sys::Function>()
            .ok()?
            .call0(&performance)
            .ok()?
            .as_f64()
    };
    now().unwrap_or_else(js_sys::Date::now)
}

/// Whether we run on the main thread (with a DOM).
pub fn has_document() -> bool {
    web_sys::window().is_some_and(|w| w.document().is_some())
}

pub fn set_interval(callback: &js_sys::Function, ms: i32) -> Result<i32, JsValue> {
    Ok(call("setInterval", &[callback, &JsValue::from(ms)])?
        .as_f64()
        .unwrap_or(0.0) as i32)
}

pub fn clear_interval(id: i32) {
    let _ = call("clearInterval", &[&JsValue::from(id)]);
}

pub fn request_animation_frame(callback: &js_sys::Function) -> Result<i32, JsValue> {
    Ok(call("requestAnimationFrame", &[callback])?
        .as_f64()
        .unwrap_or(0.0) as i32)
}

pub fn cancel_animation_frame(id: i32) {
    let _ = call("cancelAnimationFrame", &[&JsValue::from(id)]);
}

pub fn create_image_bitmap(
    source: &JsValue,
    options: &JsValue,
) -> Result<js_sys::Promise, JsValue> {
    Ok(call("createImageBitmap", &[source, options])?.unchecked_into())
}

/// `devicePixelRatio` on the main thread, `None` in a worker.
pub fn device_pixel_ratio() -> Option<f64> {
    web_sys::window()
        .map(|w| w.device_pixel_ratio())
        .filter(|r| *r > 0.0)
}
