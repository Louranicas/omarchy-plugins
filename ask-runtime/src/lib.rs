//! Serialized ACP adapter. Provider-supplied metadata never grants local authority.
pub mod provider;
use ask_core::launch::{FrozenLaunch, Harness};
use ask_core::permission::{Kind, Offered, Outcome};
use ask_core::session::{ConfigOption, Effect, Session, Tool, ToolStatus};
use ask_core::{Error as CoreError, Id, RequestId, Text};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::atomic::AtomicBool;
use std::time::Duration;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Core(CoreError),
    Transport(acp_transport::Error),
    Protocol,
    Provider,
    Limit,
    Stale,
    Closed,
    EntropyUnavailable,
}
impl From<CoreError> for Error {
    fn from(e: CoreError) -> Self {
        Self::Core(e)
    }
}
impl From<acp_transport::Error> for Error {
    fn from(e: acp_transport::Error) -> Self {
        Self::Transport(e)
    }
}
/// Opaque runtime identity; capture with UI callbacks, never substitute on delivery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Instance([u8; 16]);
impl Instance {
    fn fresh() -> Result<Self, Error> {
        let mut bytes = [0u8; 16];
        let mut at = 0;
        for _ in 0..8 {
            let n = unsafe {
                libc::getrandom(
                    bytes[at..].as_mut_ptr().cast(),
                    bytes.len() - at,
                    libc::GRND_NONBLOCK,
                )
            };
            if n > 0 {
                at += n as usize;
                if at == bytes.len() {
                    return Ok(Self(bytes));
                }
            } else if n < 0
                && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted
            {
                continue;
            } else {
                break;
            }
        }
        Err(Error::EntropyUnavailable)
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Ready,
    Changed,
    Attention,
    Permission {
        instance: Instance,
        generation: u64,
        epoch: u64,
        request: RequestId,
        title: Text,
        options: Vec<Offered>,
    },
    ReapOutstanding,
    Terminate,
    Fault(Error),
    Reaped,
}
#[derive(Clone)]
enum Pending {
    Initialize(u64),
    New(u64),
    Config(u64),
    Prompt(Id),
    Steer(Id, u64),
    Close,
}
pub struct Adapter {
    instance: Instance,
    current_mode: Option<Id>,
    session: Session,
    calls: BTreeMap<String, Pending>,
    call_sequence: u64,
    update_sequence: u64,
    options: Vec<ConfigOption>,
    close_supported: bool,
    outbox: Vec<Value>,
    events: Vec<Event>,
    poisoned: bool,
    cancelling: bool,
}
impl Adapter {
    pub fn new(launch: FrozenLaunch, now: u64) -> Result<Self, Error> {
        let (session, initial) = Session::new(launch, 1, now)?;
        let mut this = Self {
            instance: Instance::fresh()?,
            current_mode: None,
            session,
            calls: BTreeMap::new(),
            call_sequence: 0,
            update_sequence: 0,
            options: vec![],
            close_supported: false,
            outbox: vec![],
            events: vec![],
            poisoned: false,
            cancelling: false,
        };
        this.effects(vec![initial])?;
        Ok(this)
    }
    pub fn instance(&self) -> Instance {
        self.instance
    }
    pub fn current_mode(&self) -> Option<&Id> {
        self.current_mode.as_ref()
    }
    pub fn config_options(&self) -> &[ConfigOption] {
        &self.options
    }
    pub fn session(&self) -> &Session {
        &self.session
    }
    pub fn take_outbox(&mut self) -> Vec<Value> {
        std::mem::take(&mut self.outbox)
    }
    pub fn take_events(&mut self) -> Vec<Event> {
        std::mem::take(&mut self.events)
    }
    fn emit(&mut self, v: Value) -> Result<(), Error> {
        if self.outbox.len() >= 64 {
            return Err(Error::Limit);
        };
        self.outbox.push(v);
        Ok(())
    }
    fn event(&mut self, e: Event) -> Result<(), Error> {
        if self.events.len() >= 64 {
            return Err(Error::Limit);
        };
        self.events.push(e);
        Ok(())
    }
    fn call(&mut self, method: &str, params: Value, pending: Pending) -> Result<(), Error> {
        if self.calls.len() >= 64 {
            return Err(Error::Limit);
        };
        self.call_sequence = self.call_sequence.checked_add(1).ok_or(Error::Limit)?;
        let id = format!("ask:{}:{}", self.session.generation(), self.call_sequence);
        self.emit(json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))?;
        self.calls.insert(id, pending);
        Ok(())
    }
    fn effects(&mut self, effects: Vec<Effect>) -> Result<(), Error> {
        for e in effects {
            match e{
  Effect::Initialize{operation,protocol_version,..}=>self.call("initialize",json!({"protocolVersion":protocol_version,"clientCapabilities":{"session":{"configOptions":{}}}}),Pending::Initialize(operation))?,
  Effect::NewSession{operation,..}=>{
   let launch=self.session.launch();let mut params=json!({"cwd":launch.cwd.to_str().ok_or(Error::Protocol)?,"mcpServers":[]});
   if launch.harness==Harness::Claude && let Some(model)=&launch.model {params["_meta"]=json!({"claudeCode":{"options":{"model":model.as_str(),"settings":{"model":model.as_str(),"availableModels":[model.as_str()]}}}})};
   self.call("session/new",params,Pending::New(operation))?;
  },
  Effect::SetConfig{operation,session,config,value,..}=>self.call("session/set_config_option",json!({"sessionId":session.as_str(),"configId":config.as_str(),"value":value.as_str()}),Pending::Config(operation))?,
  Effect::Ready{..}=>self.event(Event::Ready)?,
  Effect::Prompt{session,turn,text,..}=>self.call("session/prompt",json!({"sessionId":session.as_str(),"prompt":[{"type":"text","text":text.as_str()}]}),Pending::Prompt(turn))?,
  Effect::Steer{session,turn,operation,text,..}=>self.call("_session/steering",json!({"sessionId":session.as_str(),"prompt":[{"type":"text","text":text.as_str()}]}),Pending::Steer(turn,operation))?,
  Effect::Cancel{session,..}=>self.emit(json!({"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":session.as_str()}}))?,
  Effect::Permission(response)=>{let outcome=match response.outcome{Outcome::Selected(id)=>json!({"outcome":"selected","optionId":id.as_str()}),Outcome::Cancelled=>json!({"outcome":"cancelled"})};self.emit(json!({"jsonrpc":"2.0","id":wire_id(&response.request),"result":{"outcome":outcome}}))?},
  Effect::CloseSession{session,..}=>if self.close_supported{self.call("session/close",json!({"sessionId":session.as_str()}),Pending::Close)?},
  Effect::RequestAttention=>self.event(Event::Attention)?,
  Effect::SignalTerm{..}=>self.event(Event::Terminate)?,
  Effect::SignalKill{..}|Effect::ReapOutstanding{..}=>self.event(Event::ReapOutstanding)?,
 }
        }
        Ok(())
    }
    fn alive(&self) -> Result<(), Error> {
        if self.poisoned {
            Err(Error::Closed)
        } else {
            Ok(())
        }
    }
    pub fn submit(&mut self, turn: Id, text: Text, now: u64) -> Result<(), Error> {
        self.alive()?;
        let e = self
            .session
            .submit(self.session.generation(), turn, text, now)?;
        self.update_sequence = 0;
        self.cancelling = false;
        self.effects(vec![e])
    }
    pub fn steer(&mut self, text: Text, now: u64) -> Result<(), Error> {
        self.alive()?;
        let turn = self
            .session
            .active_turn()
            .cloned()
            .ok_or(CoreError::NoActiveTurn)?;
        let effect = self
            .session
            .steer(self.session.generation(), &turn, text, now)?;
        self.effects(vec![effect])
    }
    pub fn cancel(&mut self, now: u64) -> Result<(), Error> {
        self.alive()?;
        let turn = self
            .session
            .active_turn()
            .cloned()
            .ok_or(CoreError::NoActiveTurn)?;
        let e = self
            .session
            .cancel_turn(self.session.generation(), &turn, now)?;
        self.cancelling = true;
        self.effects(e)
    }
    pub fn answer(
        &mut self,
        instance: Instance,
        generation: u64,
        epoch: u64,
        request: &RequestId,
        option: Option<&Id>,
        now: u64,
    ) -> Result<(), Error> {
        self.alive()?;
        if instance != self.instance {
            return Err(Error::Stale);
        }
        let e = self
            .session
            .answer_permission(generation, epoch, request, option, now)?;
        self.effects(vec![e])
    }
    pub fn pin(&mut self) -> Result<(), Error> {
        self.session.pin()?;
        Ok(())
    }
    pub fn set_focused(&mut self, focused: bool) {
        self.session.set_focused(focused)
    }
    pub fn close(&mut self, now: u64) -> Result<(), Error> {
        let previous = self.session.phase();
        let e = self.session.close(now)?;
        self.lifecycle_effects(previous, e)
    }
    pub fn tick(&mut self, now: u64) -> Result<(), Error> {
        let previous = self.session.phase();
        let e = self.session.tick(now)?;
        self.lifecycle_effects(previous, e)
    }
    fn lifecycle_effects(
        &mut self,
        previous: ask_core::session::Phase,
        effects: Vec<Effect>,
    ) -> Result<(), Error> {
        use ask_core::session::Phase;
        if !matches!(previous, Phase::Closing(_) | Phase::Closed)
            && matches!(self.session.phase(), Phase::Closing(_))
        {
            // Closing revokes unsent work. Keep shutdown effects from an earlier
            // close/tick, and retain correlation only for requests already sent.
            for value in self.outbox.drain(..) {
                if value.get("method").is_some()
                    && let Some(id) = value.get("id").and_then(Value::as_str)
                {
                    self.calls.remove(id);
                }
            }
        }
        self.effects(effects)
    }
    pub fn lost(&mut self, now: u64) {
        self.poisoned = true;
        self.calls.clear();
        self.outbox.clear();
        let _ = self.session.lost(self.session.generation(), now);
    }
    pub fn incoming(
        &mut self,
        instance: Instance,
        generation: u64,
        value: Value,
        now: u64,
    ) -> Result<(), Error> {
        self.alive()?;
        if instance != self.instance || generation != self.session.generation() {
            return Err(Error::Stale);
        };
        let result = self.handle(value, now);
        if result.is_err() {
            self.lost(now)
        }
        result
    }
    fn handle(&mut self, value: Value, now: u64) -> Result<(), Error> {
        acp_transport::encode(&value)?;
        if let Some(method) = value.get("method").and_then(Value::as_str) {
            return self.notification(method, &value, now);
        }
        let id = value
            .get("id")
            .and_then(Value::as_str)
            .ok_or(Error::Protocol)?;
        let pending = self.calls.remove(id).ok_or(Error::Stale)?;
        if value.get("error").is_some() {
            return Err(Error::Provider);
        };
        let r = value
            .get("result")
            .filter(|v| v.is_object())
            .ok_or(Error::Protocol)?;
        let generation = self.session.generation();
        let effects = match pending {
            Pending::Initialize(op) => {
                self.close_supported = r
                    .pointer("/agentCapabilities/sessionCapabilities/close")
                    .is_some_and(Value::is_object);
                let version = r["protocolVersion"]
                    .as_u64()
                    .and_then(|n| u32::try_from(n).ok())
                    .ok_or(Error::Protocol)?;
                vec![self.session.initialized(
                    generation,
                    op,
                    version,
                    r.pointer("/_meta/steering/supported") == Some(&Value::Bool(true)),
                    now,
                )?]
            }
            Pending::New(op) => {
                self.options = parse_options(r.get("configOptions"))?;
                vec![self.session.created(
                    generation,
                    op,
                    id_field(r, "sessionId")?,
                    self.options.clone(),
                    now,
                )?]
            }
            Pending::Config(op) => {
                if r.get("configOptions").is_some() {
                    self.options = parse_options(r.get("configOptions"))?
                };
                vec![
                    self.session
                        .configured(generation, op, self.options.clone(), now)?,
                ]
            }
            Pending::Prompt(turn) => self.session.finished_reason(
                generation,
                &turn,
                Id::new(
                    r.get("stopReason")
                        .and_then(Value::as_str)
                        .unwrap_or("end_turn"),
                )?,
                now,
            )?,
            Pending::Steer(turn, op) => {
                if r["outcome"].as_str().is_none_or(|s| s == "failed") {
                    return Err(Error::Provider);
                };
                self.session.steered(generation, &turn, op, now)?;
                vec![]
            }
            Pending::Close => vec![],
        };
        self.effects(effects)?;
        self.event(Event::Changed)
    }
    fn notification(&mut self, method: &str, value: &Value, now: u64) -> Result<(), Error> {
        let p = value
            .get("params")
            .filter(|p| p.is_object())
            .ok_or(Error::Protocol)?;
        if method != "session/update" && method != "session/request_permission" {
            if let Some(id) = value.get("id") {
                self.emit(json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":"Method not supported"}}))?;
            }
            return Ok(());
        }
        if self.session.provider_session() != Some(&id_field(p, "sessionId")?) {
            return Err(Error::Stale);
        }
        if method == "session/update" {
            if value.get("id").is_some() {
                return Err(Error::Protocol);
            }
            let update = &p["update"];
            match update["sessionUpdate"].as_str().ok_or(Error::Protocol)? {
                "current_mode_update" => {
                    self.current_mode = Some(id_field(update, "currentModeId")?);
                    return self.event(Event::Changed);
                }
                "config_option_update" => {
                    self.options = parse_options(update.get("configOptions"))?;
                    return self.event(Event::Changed);
                }
                "agent_message_chunk"
                | "agent_thought_chunk"
                | "tool_call"
                | "tool_call_update" => {}
                _ => return Ok(()),
            }
        }
        let turn = self
            .session
            .active_turn()
            .cloned()
            .ok_or(CoreError::NoActiveTurn)?;
        let generation = self.session.generation();
        if self.cancelling {
            if method == "session/request_permission" {
                let request = parse_id(value.get("id").ok_or(Error::Protocol)?)?;
                self.emit(json!({"jsonrpc":"2.0","id":wire_id(&request),"result":{"outcome":{"outcome":"cancelled"}}}))?;
            } else if value.get("id").is_some() {
                return Err(Error::Protocol);
            }
            return Ok(());
        }
        if method == "session/request_permission" {
            let request = parse_id(value.get("id").ok_or(Error::Protocol)?)?;
            let raw = p["options"]
                .as_array()
                .filter(|v| v.len() <= 32)
                .ok_or(Error::Protocol)?;
            let mut options = vec![];
            for o in raw {
                options.push(Offered {
                    id: id_field(o, "optionId")?,
                    kind: match o["kind"].as_str() {
                        Some("allow_once") => Kind::AllowOnce,
                        Some("reject_once") => Kind::RejectOnce,
                        Some("allow_always") => Kind::AllowAlways,
                        Some("reject_always") => Kind::RejectAlways,
                        _ => return Err(Error::Protocol),
                    },
                })
            }
            // No Context: untrusted provider labels do not attest project/capability authority.
            if let Some(e) = self.session.permission(
                generation,
                &turn,
                request.clone(),
                options.clone(),
                None,
                now,
            )? {
                self.effects(vec![e])?
            } else {
                let title = p
                    .pointer("/toolCall/title")
                    .and_then(Value::as_str)
                    .unwrap_or("Use a tool");
                if title.len() > 4096 {
                    return Err(Error::Limit);
                };
                self.event(Event::Permission {
                    instance: self.instance,
                    generation,
                    epoch: self.session.permissions().permission_epoch(),
                    request,
                    title: Text::new(title)?,
                    options,
                })?
            }
            return Ok(());
        }
        if value.get("id").is_some() {
            return Err(Error::Protocol);
        };
        let u = &p["update"];
        let kind = u["sessionUpdate"].as_str().ok_or(Error::Protocol)?;
        if !matches!(
            kind,
            "agent_message_chunk" | "agent_thought_chunk" | "tool_call" | "tool_call_update"
        ) {
            return Ok(());
        };
        let seq = self.update_sequence.checked_add(1).ok_or(Error::Limit)?;
        match kind {
            "agent_message_chunk" => {
                if u.pointer("/content/type").and_then(Value::as_str) != Some("text") {
                    return Ok(());
                };
                let message = u
                    .get("messageId")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .map(Id::new)
                    .transpose()?;
                self.session.chunk(
                    generation,
                    &turn,
                    seq,
                    message,
                    Text::new(
                        u.pointer("/content/text")
                            .and_then(Value::as_str)
                            .ok_or(Error::Protocol)?,
                    )?,
                    now,
                )?;
            }
            "agent_thought_chunk" => self.session.thinking_update(generation, &turn, seq, now)?,
            _ => {
                let tool = id_field(u, "toolCallId")?;
                let previous = self.session.tools().find(|t| t.id == tool);
                let title = u["title"]
                    .as_str()
                    .map(str::to_owned)
                    .or_else(|| previous.map(|t| t.title.as_str().to_owned()))
                    .unwrap_or_else(|| "Using a tool".into());
                let status = match u["status"].as_str() {
                    Some("pending") => ToolStatus::Pending,
                    Some("in_progress") => ToolStatus::InProgress,
                    Some("completed") => ToolStatus::Completed,
                    Some("failed") => ToolStatus::Failed,
                    None => previous.map(|t| t.status).unwrap_or(ToolStatus::InProgress),
                    _ => return Err(Error::Protocol),
                };
                self.session.tool_update(
                    generation,
                    &turn,
                    seq,
                    Tool {
                        id: tool,
                        title: Text::new(title)?,
                        status,
                    },
                    now,
                )?;
            }
        };
        self.update_sequence = seq;
        self.event(Event::Changed)
    }
}
fn id_field(v: &Value, key: &str) -> Result<Id, Error> {
    Ok(Id::new(v[key].as_str().ok_or(Error::Protocol)?)?)
}
fn parse_id(v: &Value) -> Result<RequestId, Error> {
    if let Some(n) = v.as_i64() {
        Ok(RequestId::Number(n))
    } else if let Some(s) = v.as_str() {
        Ok(RequestId::String(Id::new(s)?))
    } else {
        Err(Error::Protocol)
    }
}
fn wire_id(id: &RequestId) -> Value {
    match id {
        RequestId::Number(n) => json!(n),
        RequestId::String(s) => json!(s.as_str()),
    }
}
fn parse_options(value: Option<&Value>) -> Result<Vec<ConfigOption>, Error> {
    let Some(value) = value else {
        return Ok(vec![]);
    };
    let raw = value
        .as_array()
        .filter(|v| v.len() <= 64)
        .ok_or(Error::Protocol)?;
    let mut result = vec![];
    for o in raw {
        // Only select controls are implemented/advertised; unknown and boolean
        // controls do not become invented model or permission choices.
        if o.get("type")
            .is_some_and(|kind| kind.as_str() != Some("select"))
        {
            continue;
        }
        let mut values = vec![];
        let options = o["options"]
            .as_array()
            .filter(|v| v.len() <= 256)
            .ok_or(Error::Protocol)?;
        for item in options {
            if let Some(group) = item.get("options") {
                for entry in group
                    .as_array()
                    .filter(|v| v.len() <= 256)
                    .ok_or(Error::Protocol)?
                {
                    values.push(id_field(entry, "value")?);
                    if values.len() > 256 {
                        return Err(Error::Limit);
                    }
                }
            } else {
                values.push(id_field(item, "value")?)
            }
        }
        result.push(ConfigOption {
            id: id_field(o, "id")?,
            category: o
                .get("category")
                .map(|v| Id::new(v.as_str().ok_or(Error::Protocol)?).map_err(Error::from))
                .transpose()?,
            values,
        });
    }
    Ok(result)
}
/// Worker-owned live transport; pump off the UI thread. Policy remains Ask until
/// a separately verified durable policy writer is integrated.
pub struct Worker {
    adapter: Adapter,
    transport: acp_transport::Transport,
    stopped: bool,
    reap_reported: bool,
}
impl Worker {
    pub fn spawn(
        launch: FrozenLaunch,
        environment: Vec<(std::ffi::OsString, std::ffi::OsString)>,
        now: u64,
    ) -> Result<Self, Error> {
        let argv = launch.command.args();
        let transport = acp_transport::Transport::spawn(&acp_transport::Launch {
            executable: argv[0].as_str().into(),
            arguments: argv[1..].iter().map(Into::into).collect(),
            directory: launch.cwd.clone(),
            environment,
        })?;
        let adapter = Adapter::new(launch, now)?;
        Ok(Self {
            adapter,
            transport,
            stopped: false,
            reap_reported: false,
        })
    }
    pub fn adapter(&self) -> &Adapter {
        &self.adapter
    }
    pub fn adapter_mut(&mut self) -> &mut Adapter {
        &mut self.adapter
    }
    pub fn pump(&mut self, now: u64, cancel: &AtomicBool) -> Result<Vec<Event>, Error> {
        if self.stopped {
            if !self.reap_reported
                && self.transport.reap_receipt().state() == acp_transport::ReapState::Reaped
            {
                self.reap_reported = true;
                self.adapter
                    .session
                    .child_exited(self.adapter.session.generation(), now)?;
                return Ok(vec![Event::Reaped]);
            }
            return Ok(vec![]);
        }
        let deadline = std::time::Instant::now() + Duration::from_millis(250);
        let result = (|| {
            self.adapter.tick(now)?;
            for v in self.adapter.take_outbox() {
                self.transport.send(
                    &v,
                    deadline.saturating_duration_since(std::time::Instant::now()),
                    cancel,
                )?
            }
            for _ in 0..32 {
                let Some(v) = self.transport.poll_receive(cancel)? else {
                    break;
                };
                self.adapter.incoming(
                    self.adapter.instance,
                    self.adapter.session.generation(),
                    v,
                    now,
                )?;
                for v in self.adapter.take_outbox() {
                    self.transport.send(
                        &v,
                        deadline.saturating_duration_since(std::time::Instant::now()),
                        cancel,
                    )?
                }
            }
            Ok::<(), Error>(())
        })();
        let mut events = self.adapter.take_events();
        let result = result.and_then(|()| {
            if events.contains(&Event::Terminate) {
                self.transport.terminate()?;
            }
            Ok(())
        });
        if let Err(error) = result {
            // Deliver already validated updates before the terminal event. A child
            // exiting immediately after its final reply must not erase that reply.
            if !matches!(
                self.adapter.session.phase(),
                ask_core::session::Phase::Closing(_)
            ) {
                self.adapter.lost(now);
            }
            events.push(Event::Fault(error));
            self.stopped = true;
            self.transport.close();
        } else if events.contains(&Event::ReapOutstanding) {
            self.stopped = true;
            self.transport.close();
        }
        Ok(events)
    }
    pub fn reap_receipt(&self) -> acp_transport::ReapReceipt {
        self.transport.reap_receipt()
    }
}
