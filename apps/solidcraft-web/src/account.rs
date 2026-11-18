//! SolidCraft design operations through the WebOS account host.

use std::{cell::RefCell, rc::Rc};

use serde_json::{Value, json};
use solidcraft_engine::{Session, cmd::PreparedAccountSave};
use solidcraft_ui_egui::{AccountDesignFn, AccountDesignRequest, SolveApp};
use wasm_bindgen::JsValue;

use crate::account_port::{AccountPort, field, string};

pub(crate) struct AccountView {
    port: AccountPort,
    action: Rc<RefCell<Option<AccountDesignRequest>>>,
    last_state: Value,
    pending: Option<Save>,
    dialog: Option<(String, Option<u64>)>,
    sequence: u64,
    lease: Option<String>,
    finalized: bool,
}

struct Save {
    ticket: String,
    document: String,
    revision: String,
    path: String,
    project: PreparedAccountSave,
}

impl AccountView {
    pub(crate) fn new(port: AccountPort) -> Self {
        Self { port, action: Rc::default(), last_state: Value::Null, pending: None, dialog: None, sequence: 0, lease: None, finalized: false }
    }

    pub(crate) fn design_service(&self) -> AccountDesignFn {
        let action = self.action.clone();
        Box::new(move |request| {
            let mut next = action.try_borrow_mut().map_err(|_| "SolidCraft is processing a project request.")?;
            if next.is_some() {
                return Err("SolidCraft is processing a project request.".into());
            }
            *next = Some(request);
            Ok(())
        })
    }

    pub(crate) fn suspended(&self) -> bool {
        self.lease.is_some() || self.finalized
    }

    fn busy(&self) -> bool {
        self.pending.is_some() || self.dialog.is_some()
    }

    fn unfinished(app: &SolveApp) -> bool {
        app.tool.is_some() || app.dialog.is_some() || app.session.moved_uncaptured()
    }

    fn state(&self, app: &SolveApp) -> Value {
        let session = &app.session;
        let unfinished = Self::unfinished(app);
        json!({"available":true,"documentId":session.uid.to_string(),"revision":session.revision.to_string(),
            "recoveryRevision":session.revision.to_string(),"dirty":session.is_dirty(),"capturable":!unfinished,
            "unfinishedEdit":unfinished,"autosaveEligible":false,"operationPending":self.busy()})
    }

    fn targets(app: &SolveApp, packet: &JsValue) -> bool {
        let session = &app.session;
        number(packet, "documentId") == Some(session.uid) && number(packet, "revision") == Some(session.revision)
    }

    pub(crate) fn poll(&mut self, app: &mut SolveApp) {
        for (kind, packet) in self.port.take_messages() {
            if let Err(error) = self.receive(app, &kind, &packet) {
                app.set_status(error, true);
            }
        }
        let action = self.action.borrow_mut().take();
        if let Some(action) = action
            && let Err(error) = self.project_action(app, action)
        {
            app.set_status(error, true);
        }
    }

    pub(crate) fn publish_state(&mut self, app: &SolveApp) -> Result<(), String> {
        let state = self.state(app);
        if state != self.last_state {
            self.port.publish("project-state", state.clone(), None)?;
            self.last_state = state;
        }
        Ok(())
    }

    fn receive(&mut self, app: &mut SolveApp, kind: &str, packet: &JsValue) -> Result<(), String> {
        if kind == "save-result" {
            return self.settle_save(app, packet);
        }
        let Some(id) = bounded(packet, "requestId", 36) else { return Ok(()) };
        match kind {
            "recovery-capture" => {
                let mut reply = json!({"requestId":id,"documentId":bounded(packet,"documentId",20),"revision":bounded(packet,"revision",20)});
                if Self::targets(app, packet) && !Self::unfinished(app) && !self.suspended() {
                    match app.session.capture_account_draft() {
                        Ok(bytes) => return self.port.publish("recovery-captured", reply, Some(bytes)),
                        Err(error) => reply["message"] = json!(error.to_string()),
                    }
                } else {
                    // Let the host retire this draft request before its correlated reply arrives.
                    self.publish_state(app)?;
                    reply["message"] = json!("The SolidCraft project changed before recovery capture.");
                }
                self.port.publish("recovery-captured", reply, None)
            }
            "session-save" => self.capture_session_save(app, packet, id),
            "session" => self.session(app, packet, id),
            "dialog-result" => self.dialog_result(app, packet, id),
            "operation" => self.operation(app, packet, id),
            _ => Ok(()),
        }
    }

    fn next_id(&mut self) -> Result<String, String> {
        self.sequence = self.sequence.checked_add(1).ok_or("SolidCraft exhausted its request identifiers.")?;
        Ok(self.sequence.to_string())
    }

    fn request_dialog(&mut self, mut packet: Value, revision: Option<u64>) -> Result<(), String> {
        let id = self.next_id()?;
        packet["requestId"] = json!(id);
        self.port.publish("dialog", packet, None)?;
        self.dialog = Some((id, revision));
        Ok(())
    }

    fn project_action(&mut self, app: &mut SolveApp, action: AccountDesignRequest) -> Result<(), String> {
        if self.suspended() || self.busy() {
            return Err("Finish the pending SolidCraft project operation first.".into());
        }
        let session = &app.session;
        match action {
            AccountDesignRequest::Close => self.port.publish("close-intent", json!({}), None),
            AccountDesignRequest::New => self.request_dialog(json!({"operation":"new","settings":"{}"}), None),
            AccountDesignRequest::Open(path) => self.request_dialog(json!({"operation":"open","path":path}), None),
            AccountDesignRequest::Save | AccountDesignRequest::SaveAs(_) => {
                if Self::unfinished(app) {
                    return Err("Finish the current SolidCraft design command before saving.".into());
                }
                let suggested = match action {
                    AccountDesignRequest::Save => {
                        if let Some(path) = &session.path {
                            return self.send_save(app, path.clone(), "save", json!({}));
                        }
                        format!("{}.sc", session.save_as_name())
                    }
                    AccountDesignRequest::SaveAs(name) => name,
                    _ => return Err("Invalid SolidCraft save operation.".into()),
                };
                self.request_dialog(json!({"operation":"save","suggested":suggested}), Some(session.revision))
            }
        }
    }

    fn dialog_result(&mut self, app: &SolveApp, packet: &JsValue, id: String) -> Result<(), String> {
        if !self.dialog.as_ref().is_some_and(|(expected, _)| expected == &id) {
            return Ok(());
        }
        let Some((_, revision)) = self.dialog.take() else { return Ok(()) };
        match bounded(packet, "outcome", 16).as_deref() {
            Some("failed") => Err(bounded(packet, "message", 250).unwrap_or_else(|| "The SolidCraft dialog failed.".into())),
            Some("destination") if revision.is_some() => {
                if revision != Some(app.session.revision) || Self::unfinished(app) || self.suspended() {
                    return Err("The project changed while choosing its save destination.".into());
                }
                let path = bounded(packet, "path", 2048).ok_or("Invalid SolidCraft save destination.")?;
                self.send_save(app, path, "save", json!({}))
            }
            _ => Ok(()),
        }
    }

    fn capture_session_save(&mut self, app: &SolveApp, packet: &JsValue, id: String) -> Result<(), String> {
        let path = bounded(packet, "path", 2048).ok_or("Invalid SolidCraft save destination.")?;
        let mut reply = json!({"requestId":id,"path":path,"documentId":bounded(packet,"documentId",20),
            "reviewedRevision":bounded(packet,"revision",20)});
        if self.busy() || self.suspended() || Self::unfinished(app) || !Self::targets(app, packet) {
            reply["outcome"] = json!("busy");
            return self.port.publish("session-save-result", reply, None);
        }
        if let Err(error) = self.send_save(app, path, "session-save-result", reply.clone()) {
            reply["outcome"] = json!("failed");
            reply["message"] = json!(error);
            self.port.publish("session-save-result", reply, None)?;
        }
        Ok(())
    }

    fn send_save(&mut self, app: &SolveApp, path: String, kind: &str, mut reply: Value) -> Result<(), String> {
        let (project, bytes) = app.session.prepare_account_save(&path).map_err(|error| error.to_string())?;
        let ticket = self.next_id()?;
        let document = app.session.uid.to_string();
        let revision = app.session.revision.to_string();
        reply["outcome"] = json!("captured");
        reply["ticket"] = json!(ticket);
        reply["documentId"] = json!(document);
        reply["revision"] = json!(revision);
        reply["path"] = json!(path);
        self.port.publish(kind, reply, Some(bytes))?;
        self.pending = Some(Save { ticket, document, revision, path, project });
        Ok(())
    }

    fn settle_save(&mut self, app: &mut SolveApp, packet: &JsValue) -> Result<(), String> {
        let matching = self.pending.as_ref().is_some_and(|save| {
            bounded(packet, "ticket", 20).as_deref() == Some(save.ticket.as_str())
                && bounded(packet, "documentId", 20).as_deref() == Some(save.document.as_str())
                && bounded(packet, "revision", 20).as_deref() == Some(save.revision.as_str())
                && bounded(packet, "path", 2048).as_deref() == Some(save.path.as_str())
        });
        let outcome = bounded(packet, "outcome", 16);
        if !matching || !matches!(outcome.as_deref(), Some("written" | "failed" | "unknown")) {
            return Ok(());
        }
        let Some(save) = self.pending.take() else { return Ok(()) };
        let adopted = outcome.as_deref() == Some("written") && app.session.adopt_account_save(save.project).is_ok();
        self.port.publish(
            "save-settled",
            json!({"ticket":save.ticket,"documentId":save.document,"revision":save.revision,
            "path":save.path,"adopted":adopted,"projectState":self.state(app)}),
            None,
        )
    }

    fn session(&mut self, app: &mut SolveApp, packet: &JsValue, id: String) -> Result<(), String> {
        let operation = bounded(packet, "operation", 16).ok_or("Invalid SolidCraft session operation.")?;
        let lease = bounded(packet, "lease", 64).filter(|value| !value.is_empty()).ok_or("Invalid SolidCraft session lease.")?;
        let accepted = match operation.as_str() {
            "pause"
                if !self.suspended()
                    && !self.busy()
                    && !Self::unfinished(app)
                    && Self::targets(app, packet)
                    && field(packet, "dirty").as_bool() == Some(app.session.is_dirty())
                    && field(packet, "unfinishedEdit").as_bool() == Some(false) =>
            {
                self.lease = Some(lease.clone());
                true
            }
            "release" | "finalize" if self.lease.as_deref() == Some(lease.as_str()) => {
                self.lease = None;
                self.finalized = operation == "finalize";
                true
            }
            _ => false,
        };
        self.port.publish(
            "session-result",
            json!({"requestId":id,"operation":operation,"lease":lease,"ok":accepted,
            "message":if accepted {""} else {"The SolidCraft project changed after close review."}}),
            None,
        )
    }

    fn operation(&self, app: &mut SolveApp, packet: &JsValue, id: String) -> Result<(), String> {
        let document = bounded(packet, "documentId", 20).ok_or("Invalid SolidCraft project target.")?;
        let mut reply = json!({"requestId":id,"documentId":document});
        let path = field(packet, "path");
        let path = if path.is_null() { None } else { Some(path.as_string().ok_or("Invalid SolidCraft project binding.")?) };
        let session = &app.session;
        let result = if self.suspended() || document != session.uid.to_string() || path != session.path {
            Err("The SolidCraft operation does not match the current design.".into())
        } else {
            let command = bounded(packet, "command", 64).ok_or("Invalid SolidCraft project operation.")?;
            let args = bounded(packet, "args", 4096).ok_or("Invalid SolidCraft operation arguments.")?;
            let args: Value = serde_json::from_str(&args).map_err(|_| "Invalid SolidCraft operation arguments.")?;
            let fields = args.as_object().ok_or("SolidCraft operation arguments must be an object.")?;
            if command == "document.inspect" && fields.is_empty() {
                Ok(json!({"title":session.doc.name,"featureCount":session.doc.features.len(),"dirty":session.is_dirty()}))
            } else if self.busy() || Self::unfinished(app) {
                Err("Finish the pending SolidCraft design command first.".into())
            } else {
                match command.as_str() {
                    "inspect.measure" | "sketch.finish" | "edit.undo" if fields.is_empty() => app.run(&command, args),
                    "sketch.create" if fields.len() == 1 && matches!(args.get("plane").and_then(Value::as_str), Some("XY" | "XZ" | "YZ")) => {
                        app.run(&command, args)
                    }
                    "sketch.rectangle.two_point" if fields.len() == 2 && args.get("p0").is_some_and(point) && args.get("p1").is_some_and(point) => {
                        app.run(&command, args)
                    }
                    "solid.extrude" if fields.len() == 1 && args.get("distance").is_some_and(distance) => app.run(&command, args),
                    "timeline.redefine"
                        if fields.len() == 3
                            && args.get("feature").and_then(Value::as_u64).is_some_and(|id| id > 0)
                            && args.get("command").and_then(Value::as_str) == Some("solid.extrude")
                            && args.get("params").and_then(Value::as_object).is_some_and(|params| {
                                params.len() == 4
                                    && params.get("sketch").and_then(Value::as_u64).is_some_and(|id| id > 0)
                                    && params.get("profiles").and_then(Value::as_str) == Some("all")
                                    && params.get("operation").and_then(Value::as_str) == Some("new")
                                    && params.get("distance").is_some_and(distance)
                            }) =>
                    {
                        app.run(&command, args)
                    }
                    _ => Err("Unsupported SolidCraft design operation.".into()),
                }
            }
        };
        match result {
            Ok(value) => reply["result"] = json!(value.to_string()),
            Err(error) => reply["message"] = json!(error),
        }
        self.port.publish("operation-result", reply, None)
    }
}

fn bounded(packet: &JsValue, field: &str, maximum: usize) -> Option<String> {
    string(packet, field).filter(|text| text.len() <= maximum)
}

fn number(packet: &JsValue, field: &str) -> Option<u64> {
    bounded(packet, field, 20)?.parse().ok()
}

fn point(value: &Value) -> bool {
    value
        .as_array()
        .is_some_and(|point| point.len() == 2 && point.iter().all(|axis| axis.as_f64().is_some_and(|n| n.is_finite() && n.abs() <= 1.0e9)))
}

fn distance(value: &Value) -> bool {
    value.as_f64().is_some_and(|number| number.is_finite() && number > 0.0 && number <= 1.0e6)
}

pub(crate) async fn open(port: &AccountPort) -> Result<(Session, Value), String> {
    let request = port.start().await?;
    let session = if let Some(bytes) = request.bytes {
        match Session::open_account_design(request.path.as_deref(), &bytes, request.recover) {
            Ok(session) => session,
            Err(error) => {
                let error = error.to_string();
                port.open_failed(&request.request_id, &error);
                return Err(error);
            }
        }
    } else {
        if request.settings.as_object().is_some_and(|settings| !settings.is_empty()) {
            let error = "Unsupported SolidCraft new design settings.";
            port.open_failed(&request.request_id, error);
            return Err(error.into());
        }
        Session::default()
    };
    let identity = json!({
        "requestId":request.request_id, "documentId":session.uid.to_string(),
        "revision":session.revision.to_string(), "path":session.path, "ok":true,
    });
    Ok((session, identity))
}
