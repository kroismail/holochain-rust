use colored::*;
use holochain_core::{
    action::{Action, ActionWrapper},
    network::direct_message::DirectMessage,
    nucleus::ZomeFnCall,
    signal::{Signal, SignalReceiver},
};
use neon::{context::Context, prelude::*};
use std::{
    cell::RefCell,
    collections::HashMap,
    sync::{
        mpsc::{Receiver, RecvTimeoutError, SyncSender},
        Arc, Mutex,
    },
    time::Duration,
};

type ControlSender = SyncSender<ControlMsg>;
type ControlReceiver = Receiver<ControlMsg>;

/// Possible messages used to influence the behavior of the CallBlockingTask
/// Currently the only action needed is to stop it, triggering its callback
pub enum ControlMsg {
    Stop,
}

/// A predicate function which examines an ActionWrapper to see if it is
/// the one it's looking for
type CallFxCondition = Box<Fn(&ActionWrapper) -> bool + 'static + Send>;

/// A set of closures, each of which checks for a certain condition to be met
/// (usually for a certain action to be seen). When the condition specified by the closure
/// is met, that closure is removed from the set of checks.
///
/// When the set of checks goes from non-empty to empty, send a message via `tx`
/// to the `CallBlockingTask` on the other side
struct CallFxChecker {
    tx: ControlSender,
    conditions: Vec<CallFxCondition>,
}

impl CallFxChecker {
    pub fn new(tx: ControlSender) -> Self {
        Self {
            tx,
            conditions: Vec::new(),
        }
    }

    pub fn add<F>(&mut self, f: F) -> ()
    where
        F: Fn(&ActionWrapper) -> bool + 'static + Send,
    {
        self.conditions.push(Box::new(f));
        println!(
            "\n*** Condition {}: {} -> {}",
            "ADDED".green(),
            self.conditions.len() - 1,
            self.conditions.len()
        );
    }

    pub fn run_checks(&mut self, aw: &ActionWrapper) -> bool {
        let was_empty = self.conditions.is_empty();
        let size = self.conditions.len();
        self.conditions.retain(|condition| !condition(aw));
        if size != self.conditions.len() {
            println!(
                "\n*** Condition {}: {} -> {}",
                "REMOVED".red(),
                size,
                size - 1
            );
        }
        if self.conditions.is_empty() && !was_empty {
            self.stop();
            return false;
        } else {
            return true;
        }
    }

    pub fn shutdown(&mut self) {
        self.conditions.clear();
        self.stop();
    }

    fn stop(&mut self) {
        self.tx.send(ControlMsg::Stop).unwrap();
    }
}

/// A simple Task that blocks until it receives `ControlMsg::Stop`.
/// This is used to trigger a JS Promise resolution when a ZomeFnCall's
/// side effects have all completed.
pub struct CallBlockingTask {
    pub rx: ControlReceiver,
}

impl Task for CallBlockingTask {
    type Output = ();
    type Error = String;
    type JsEvent = JsUndefined;

    fn perform(&self) -> Result<(), String> {
        while let Ok(sig) = self.rx.recv() {
            match sig {
                ControlMsg::Stop => break,
            }
        }
        Ok(())
    }

    fn complete(self, mut cx: TaskContext, result: Result<(), String>) -> JsResult<JsUndefined> {
        result.map(|_| cx.undefined()).or_else(|e| {
            let error_string = cx.string(format!("unable to initialize habitat: {}", e));
            cx.throw(error_string)
        })
    }
}

fn log(msg: &str) {
    println!("{}:\n{}\n", "(((LOG)))".bold(), msg);
}

/// A singleton which runs in a Task and is the receiver for the Signal channel.
/// - handles incoming `ZomeFnCall`s, attaching and activating a new `CallFxChecker`
/// - handles incoming Signals, running all `CallFxChecker` closures
pub struct Waiter {
    checkers: HashMap<ZomeFnCall, CallFxChecker>,
    current: Option<ZomeFnCall>,
    sender_rx: Receiver<ControlSender>,
}

impl Waiter {
    pub fn new(sender_rx: Receiver<ControlSender>) -> Self {
        Self {
            checkers: HashMap::new(),
            current: None,
            sender_rx,
        }
    }

    pub fn process_signal(&mut self, sig: Signal) {
        match sig {
            Signal::Internal(ref aw) => {
                let aw = aw.clone();
                match aw.action().clone() {
                    Action::ExecuteZomeFunction(call) => match self.sender_rx.try_recv() {
                        Ok(sender) => {
                            self.add_call(call.clone(), sender);
                            self.current_checker().unwrap().add(move |aw| {
                                if let Action::ReturnZomeFunctionResult(ref r) = *aw.action() {
                                    r.call() == call
                                } else {
                                    false
                                }
                            });
                        }
                        Err(_) => {
                            self.deactivate_current();
                            log("Waiter: deactivate_current");
                        }
                    },

                    // TODO: limit to App entry?
                    Action::Commit((entry, _)) => match self.current_checker() {
                        // TODO: is there a possiblity that this can get messed up if the same
                        // entry is committed multiple times?
                        Some(checker) => {
                            checker.add(move |aw| *aw.action() == Action::Hold(entry.clone()));
                        }
                        None => (),
                    },

                    Action::SendDirectMessage(data) => {
                        let msg_id = data.msg_id;
                        match (self.current_checker(), data.message) {
                            (Some(checker), DirectMessage::Custom(_)) => {
                                checker.add(move |aw| {
                                    [
                                        Action::ResolveDirectConnection(msg_id.clone()),
                                        Action::SendDirectMessageTimeout(msg_id.clone()),
                                    ]
                                    .contains(aw.action())
                                });
                            }
                            _ => (),
                        }
                    }

                    _ => (),
                };

                self.run_checks(&aw);
            }

            _ => (),
        };
    }

    fn run_checks(&mut self, aw: &ActionWrapper) {
        let size = self.checkers.len();
        self.checkers.retain(|_, checker| checker.run_checks(aw));
        if size != self.checkers.len() {
            println!(
                "\n{}: {} -> {}",
                "Num checkers".italic(),
                size,
                self.checkers.len()
            );
        }
    }

    fn current_checker(&mut self) -> Option<&mut CallFxChecker> {
        self.current
            .clone()
            .and_then(move |call| self.checkers.get_mut(&call))
    }

    fn add_call(&mut self, call: ZomeFnCall, tx: ControlSender) {
        let checker = CallFxChecker::new(tx);

        log("Waiter: add_call...");
        self.checkers.insert(call.clone(), checker);
        self.current = Some(call);
    }

    fn deactivate_current(&mut self) {
        self.current = None;
    }
}

/// This Task is started with the TestContainer and is stopped with the TestContainer.
/// It runs in a Node worker thread, receiving Signals and running them through
/// the Waiter. Each TestContainer spawns its own MainBackgroundTask.
pub struct MainBackgroundTask {
    /// The Receiver<Signal> for the Container
    signal_rx: SignalReceiver,
    /// The Waiter is in a RefCell because perform() uses an immutable &self reference
    waiter: RefCell<Waiter>,
    /// This Mutex is flipped from true to false from within the TestContainer
    is_running: Arc<Mutex<bool>>,
}

impl MainBackgroundTask {
    pub fn new(
        signal_rx: SignalReceiver,
        sender_rx: Receiver<ControlSender>,
        is_running: Arc<Mutex<bool>>,
    ) -> Self {
        let this = Self {
            signal_rx,
            waiter: RefCell::new(Waiter::new(sender_rx)),
            is_running,
        };
        this
    }
}

impl Task for MainBackgroundTask {
    type Output = ();
    type Error = String;
    type JsEvent = JsUndefined;

    fn perform(&self) -> Result<(), String> {
        while *self.is_running.lock().unwrap() {
            // TODO: could use channels more intelligently to stop immediately
            // rather than waiting for timeout, but it's more complicated and probably
            // involves adding some kind of control variant to the Signal enum
            match self.signal_rx.recv_timeout(Duration::from_millis(250)) {
                Ok(sig) => self.waiter.borrow_mut().process_signal(sig),
                Err(RecvTimeoutError::Timeout) => continue,
                Err(err) => return Err(err.to_string()),
            }
        }

        for (_, checker) in self.waiter.borrow_mut().checkers.iter_mut() {
            println!("{}", "Shutting down lingering checker...".magenta().bold());
            checker.shutdown();
        }
        println!("Terminating MainBackgroundTask::perform() loop");
        Ok(())
    }

    fn complete(self, mut cx: TaskContext, result: Result<(), String>) -> JsResult<JsUndefined> {
        result.or_else(|e| {
            let error_string = cx.string(format!("unable to shut down background task: {}", e));
            cx.throw(error_string)
        })?;
        println!("MainBackgroundTask shut down");
        Ok(cx.undefined())
    }
}

#[cfg(test)]
mod tests {
    use super::{Action::*, *};
    use holochain_core::{
        action::DirectMessageData, network::direct_message::CustomDirectMessage,
        nucleus::ExecuteZomeFnResponse,
    };
    use holochain_core_types::{entry::Entry, json::JsonString};
    use std::sync::mpsc::sync_channel;

    fn sig(a: Action) -> Signal {
        Signal::Internal(ActionWrapper::new(a))
    }

    fn mk_entry(ty: &'static str, content: &'static str) -> Entry {
        Entry::App(ty.into(), JsonString::from(content))
    }

    fn msg_data(msg_id: &str) -> DirectMessageData {
        DirectMessageData {
            address: "fake address".into(),
            message: DirectMessage::Custom(CustomDirectMessage {
                zome: "fake zome".into(),
                payload: Ok("fake payload".into()),
            }),
            msg_id: msg_id.into(),
            is_response: false,
        }
    }

    fn zf_call(function_name: &str) -> ZomeFnCall {
        ZomeFnCall::new("z", None, function_name, "")
    }

    fn zf_response(call: ZomeFnCall) -> ExecuteZomeFnResponse {
        ExecuteZomeFnResponse::new(call, Ok(JsonString::from("")))
    }

    fn test_waiter() -> (Waiter, Receiver<ControlMsg>) {
        let (sender_tx, sender_rx) = sync_channel(0);
        let (control_tx, control_rx) = sync_channel(0);
        let waiter = Waiter::new(sender_rx);
        sender_tx
            .send(control_tx)
            .expect("Could not send control sender");
        (waiter, control_rx)
    }

    #[test]
    fn can_await_commit_simple() {
        let (mut waiter, control_rx) = test_waiter();
        let entry = mk_entry("t1", "x");
        let call = zf_call("c1");

        waiter.process_signal(sig(ExecuteZomeFunction(call.clone())));
        waiter.process_signal(sig(Commit((entry.clone(), None))));
        waiter.process_signal(sig(Hold(entry)));
        assert!(
            control_rx.try_recv().is_err(),
            "ControlMsg::Stop message received too early!"
        );
        waiter.process_signal(sig(ReturnZomeFunctionResult(zf_response(call))));
        assert!(
            control_rx.try_recv().is_ok(),
            "ControlMsg::Stop message not received!"
        );
    }

    #[test]
    fn can_await_commit_funky_ordering() {
        let (mut waiter, control_rx) = test_waiter();
        let entry_1 = mk_entry("t1", "x");
        let entry_2 = mk_entry("t2", "y");
        let entry_3 = mk_entry("t3", "z");
        let call_1 = zf_call("c1");
        let call_2 = zf_call("c2");

        waiter.process_signal(sig(ExecuteZomeFunction(call_1.clone())));
        waiter.process_signal(sig(Commit((entry_1.clone(), None))));
        waiter.process_signal(sig(ReturnZomeFunctionResult(zf_response(call_1.clone()))));

        waiter.process_signal(sig(ExecuteZomeFunction(call_2.clone())));
        waiter.process_signal(sig(Commit((entry_2.clone(), None))));
        waiter.process_signal(sig(Commit((entry_3.clone(), None))));
        waiter.process_signal(sig(Hold(entry_1)));
        waiter.process_signal(sig(ReturnZomeFunctionResult(zf_response(call_2))));

        waiter.process_signal(sig(Hold(entry_2)));
        assert!(
            control_rx.try_recv().is_err(),
            "ControlMsg::Stop message received too early!"
        );
        waiter.process_signal(sig(Hold(entry_3)));
        assert!(
            control_rx.try_recv().is_ok(),
            "ControlMsg::Stop message not received!"
        );
    }

    #[test]
    fn can_await_direct_messages() {
        let (mut waiter, control_rx) = test_waiter();
        let _entry_1 = mk_entry("a", "x");
        let _entry_2 = mk_entry("b", "y");
        let _entry_3 = mk_entry("c", "z");
        let call_1 = zf_call("1");
        let call_2 = zf_call("2");
        let msg_id_1 = "m1";
        let msg_id_2 = "m2";

        waiter.process_signal(sig(ExecuteZomeFunction(call_1.clone())));
        waiter.process_signal(sig(SendDirectMessage(msg_data(msg_id_1))));
        waiter.process_signal(sig(ReturnZomeFunctionResult(zf_response(call_1))));

        waiter.process_signal(sig(ExecuteZomeFunction(call_2.clone())));
        waiter.process_signal(sig(SendDirectMessage(msg_data(msg_id_1))));
        waiter.process_signal(sig(ReturnZomeFunctionResult(zf_response(call_2))));

        waiter.process_signal(sig(ResolveDirectConnection(msg_id_1.to_string())));
        assert!(
            control_rx.try_recv().is_err(),
            "ControlMsg::Stop message received too early!"
        );
        waiter.process_signal(sig(SendDirectMessageTimeout(msg_id_2.to_string())));
        assert!(
            control_rx.try_recv().is_ok(),
            "ControlMsg::Stop message not received!"
        );
    }
}