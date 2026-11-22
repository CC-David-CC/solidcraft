//! Transfer the fillet's evaluated model between this editor and its one-shot Worker.

use std::{cell::RefCell, io::Cursor, rc::Rc};

use js_sys::{Array, Uint8Array};
use serde_json::Value;
use solidcraft_engine::{Session, io::DesignContainerReader};
use solidcraft_ui_egui::dialogs::{ComputeFillet, FilletResult, FilletTask};
use wasm_bindgen::{JsCast, prelude::*};
use web_sys::{ErrorEvent, MessageEvent, Window, Worker, WorkerOptions, WorkerType};

// Trunk fingerprints the bindings; WebOS uses a stable filename. Give the worker
// the very same bindings URL the active editor used for its compiled module.
#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(thread_local_v2, js_namespace = ["import", "meta"], js_name = url)]
    static BINDINGS_URL: JsValue;
}

fn calculate(bytes: &[u8], parameters: &str) -> Result<Vec<u8>, String> {
    let params: Value = serde_json::from_str(parameters).map_err(|error| error.to_string())?;
    if params.get("type").and_then(Value::as_str) != Some("curvature") {
        return Err("The worker requires a curvature fillet".into());
    }
    let mut reader = DesignContainerReader::new(Cursor::new(bytes)).map_err(|error| error.to_string())?;
    let geometry = reader.geometry().ok_or("The worker could not read the evaluated model")?;
    let doc = reader.read_document().map_err(|error| error.to_string())?;
    let mut session = Session::opened(doc, Some(&geometry));
    session.execute("solid.fillet", &params).map_err(|error| error.to_string())?;
    session.worker_model()
}

#[wasm_bindgen]
pub fn compute_curvature_fillet(bytes: &[u8], parameters: &str) -> Result<Vec<u8>, JsValue> {
    calculate(bytes, parameters).map_err(|error| JsValue::from_str(&error))
}

fn complete(result: &RefCell<Option<FilletResult>>, wake: &egui::Context, reply: Result<Vec<u8>, String>) {
    let callback = result.borrow_mut().take();
    if let Some(callback) = callback {
        callback(reply);
        wake.request_repaint();
    }
}

pub(crate) fn service(ctx: &egui::Context, window: &Window) -> ComputeFillet {
    let ctx = ctx.clone();
    let window = window.clone();
    Box::new(move |bytes, params, reply| {
        let options = WorkerOptions::new();
        options.set_type(WorkerType::Module);
        let worker = Worker::new_with_options("./fillet-worker.mjs", &options).map_err(|error| format!("Could not start the fillet worker: {error:?}"))?;
        let result = Rc::new(RefCell::new(Some(reply)));
        let receive = Closure::<dyn FnMut(MessageEvent)>::new({
            let (result, wake) = (result.clone(), ctx.clone());
            move |event: MessageEvent| {
                let data = event.data();
                let reply = if let Some(message) = data.as_string() {
                    Err(message)
                } else {
                    data.dyn_into::<Uint8Array>().map(|bytes| bytes.to_vec()).map_err(|_| "The fillet worker returned an invalid result".into())
                };
                complete(&result, &wake, reply);
            }
        });
        let failed = Closure::<dyn FnMut(ErrorEvent)>::new({
            let (result, wake) = (result.clone(), ctx.clone());
            move |event: ErrorEvent| complete(&result, &wake, Err(format!("Fillet worker stopped: {}", event.message())))
        });
        let expired = Closure::<dyn FnMut()>::new({
            let (result, wake) = (result.clone(), ctx.clone());
            move || complete(&result, &wake, Err("The fillet worker timed out".into()))
        });
        worker.set_onmessage(Some(receive.as_ref().unchecked_ref()));
        worker.set_onerror(Some(failed.as_ref().unchecked_ref()));
        let data = Uint8Array::from(bytes.as_slice());
        let request = BINDINGS_URL.with(|url| Array::of4(data.as_ref(), &JsValue::from_str(&params.to_string()), &wasm_bindgen::module(), url));
        let transfer = Array::of1(data.buffer().as_ref());
        let timer = window
            .set_timeout_with_callback_and_timeout_and_arguments_0(expired.as_ref().unchecked_ref(), 120_000)
            .map_err(|error| format!("Could not set the fillet worker timeout: {error:?}"));
        let stop = {
            let window = window.clone();
            let worker = worker.clone();
            let timer_id = timer.as_ref().ok().copied();
            move || {
                if let Some(timer_id) = timer_id {
                    window.clear_timeout_with_handle(timer_id);
                }
                worker.set_onmessage(None);
                worker.set_onerror(None);
                worker.terminate();
                drop((receive, failed, expired));
            }
        };
        if let Err(error) = timer.and_then(|_| {
            worker.post_message_with_transfer(request.as_ref(), transfer.as_ref()).map_err(|error| format!("Could not send the model to the fillet worker: {error:?}"))
        }) {
            stop();
            return Err(error);
        }
        Ok(FilletTask::new(stop))
    })
}
