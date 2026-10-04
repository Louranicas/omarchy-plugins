use crate::{
    Error, Id, RequestId, Text,
    launch::{FrozenLaunch, Harness},
    permission::{Broker, CommitToken, Context, Offered, PersistPolicy, Policy, Response},
};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

pub const STARTUP_MS: u64 = 10_000;
pub const MAX_TRANSCRIPT_BYTES: usize = 1024 * 1024;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Presentation {
    Overlay,
    Pinned,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseStage {
    Graceful,
    Term,
    Kill,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Initializing,
    Creating,
    Configuring,
    Ready,
    Lost,
    Closing(CloseStage),
    Closed,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Human,
    Assistant,
}
#[derive(Debug, Clone)]
pub struct Message {
    pub role: Role,
    pub message_id: Option<Id>,
    pub body: Text,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigOption {
    pub id: Id,
    pub category: Option<Id>,
    pub values: Vec<Id>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    Initialize {
        generation: u64,
        operation: u64,
        protocol_version: u32,
    },
    NewSession {
        generation: u64,
        operation: u64,
    },
    SetConfig {
        generation: u64,
        operation: u64,
        session: Id,
        config: Id,
        value: Id,
    },
    Ready {
        generation: u64,
        session: Id,
        steering: bool,
    },
    Prompt {
        generation: u64,
        session: Id,
        turn: Id,
        text: Text,
    },
    Steer {
        generation: u64,
        session: Id,
        turn: Id,
        operation: u64,
        text: Text,
    },
    Cancel {
        generation: u64,
        session: Id,
        turn: Id,
    },
    Permission(Response),
    CloseSession {
        generation: u64,
        session: Id,
    },
    SignalTerm {
        generation: u64,
    },
    SignalKill {
        generation: u64,
    },
    ReapOutstanding {
        generation: u64,
    },
    RequestAttention,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolStatus {
    Pending,
    InProgress,
    Completed,
    Failed,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tool {
    pub id: Id,
    pub title: Text,
    pub status: ToolStatus,
}
#[derive(Debug, Clone)]
struct Turn {
    id: Id,
    cancelling: bool,
    cancel_deadline: Option<u64>,
    sequence: u64,
    steer: Option<u64>,
    reply: usize,
    message_id: Option<Id>,
}
pub struct Session {
    generation: u64,
    launch: FrozenLaunch,
    phase: Phase,
    operation: u64,
    expected_operation: u64,
    deadline: u64,
    time: u64,
    protocol: u32,
    provider: Option<Id>,
    steering: bool,
    config_requests: VecDeque<(bool, Id)>,
    presentation: Presentation,
    focused: bool,
    turn: Option<Turn>,
    seen_turns: BTreeSet<Id>,
    messages: Vec<Message>,
    tools: BTreeMap<Id, Tool>,
    thinking: bool,
    last_stop_reason: Option<Id>,
    transcript_bytes: usize,
    permissions: Broker,
}
impl Session {
    pub fn new(launch: FrozenLaunch, protocol: u32, now: u64) -> Result<(Self, Effect), Error> {
        if protocol == 0 || !launch.cwd.is_absolute() {
            return Err(Error::InvalidConfig);
        }
        let deadline = now.checked_add(STARTUP_MS).ok_or(Error::Exhausted)?;
        let s = Self {
            generation: 1,
            launch,
            phase: Phase::Initializing,
            operation: 1,
            expected_operation: 1,
            deadline,
            time: now,
            protocol,
            provider: None,
            steering: false,
            config_requests: VecDeque::new(),
            presentation: Presentation::Overlay,
            focused: true,
            turn: None,
            seen_turns: BTreeSet::new(),
            messages: Vec::new(),
            tools: BTreeMap::new(),
            thinking: false,
            last_stop_reason: None,
            transcript_bytes: 0,
            permissions: Broker::new(1),
        };
        Ok((
            s,
            Effect::Initialize {
                generation: 1,
                operation: 1,
                protocol_version: protocol,
            },
        ))
    }
    fn check(&self, generation: u64, now: u64) -> Result<(), Error> {
        if generation != self.generation {
            return Err(Error::StaleGeneration);
        }
        if now < self.time {
            return Err(Error::ClockWentBackwards);
        }
        if self.phase == Phase::Closed {
            return Err(Error::Closed);
        }
        Ok(())
    }
    fn operation(&mut self) -> Result<u64, Error> {
        self.operation = self.operation.checked_add(1).ok_or(Error::Exhausted)?;
        Ok(self.operation)
    }
    fn startup_ack(
        &self,
        generation: u64,
        operation: u64,
        phase: Phase,
        now: u64,
    ) -> Result<(), Error> {
        self.check(generation, now)?;
        if self.phase != phase {
            return Err(Error::WrongPhase);
        }
        if operation != self.expected_operation {
            return Err(Error::StaleOperation);
        }
        if now >= self.deadline {
            return Err(Error::Timeout);
        }
        Ok(())
    }
    pub fn phase(&self) -> Phase {
        self.phase
    }
    pub fn generation(&self) -> u64 {
        self.generation
    }
    pub fn provider_session(&self) -> Option<&Id> {
        self.provider.as_ref()
    }
    pub fn launch(&self) -> &FrozenLaunch {
        &self.launch
    }
    pub fn presentation(&self) -> Presentation {
        self.presentation
    }
    pub fn messages(&self) -> &[Message] {
        &self.messages
    }
    pub fn active_turn(&self) -> Option<&Id> {
        self.turn.as_ref().map(|t| &t.id)
    }
    pub fn permissions(&self) -> &Broker {
        &self.permissions
    }
    pub fn restore_policy(&mut self, policy: Policy, revision: u64, now: u64) -> Result<(), Error> {
        self.check(self.generation, now)?;
        if self.phase != Phase::Initializing {
            return Err(Error::WrongPhase);
        }
        self.permissions = Broker::from_durable(self.generation, revision, policy, now)?;
        self.time = now;
        Ok(())
    }
    pub fn propose_policy(
        &mut self,
        generation: u64,
        expected_revision: u64,
        policy: Policy,
        now: u64,
    ) -> Result<PersistPolicy, Error> {
        self.check(generation, now)?;
        if self.phase != Phase::Ready {
            return Err(Error::WrongPhase);
        }
        let result = self
            .permissions
            .propose(generation, expected_revision, policy, now)?;
        self.time = now;
        Ok(result)
    }
    pub fn policy_committed(
        &mut self,
        token: CommitToken,
        success: bool,
        now: u64,
    ) -> Result<Vec<Effect>, Error> {
        self.check(token.generation, now)?;
        if self.phase != Phase::Ready {
            return Err(Error::WrongPhase);
        }
        let result = self.permissions.committed(token, success, now);
        self.time = now;
        result.map(|rs| rs.into_iter().map(Effect::Permission).collect())
    }
    pub fn answer_permission(
        &mut self,
        generation: u64,
        permission_epoch: u64,
        id: &RequestId,
        option: Option<&Id>,
        now: u64,
    ) -> Result<Effect, Error> {
        self.check(generation, now)?;
        if self.phase != Phase::Ready {
            return Err(Error::WrongPhase);
        }
        let result = self
            .permissions
            .answer(generation, permission_epoch, id, option)?;
        self.time = now;
        Ok(Effect::Permission(result))
    }
    pub fn pin(&mut self) -> Result<(), Error> {
        if matches!(self.phase, Phase::Closing(_) | Phase::Closed) {
            return Err(Error::Closed);
        }
        self.presentation = Presentation::Pinned;
        Ok(())
    }
    pub fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
    }
    pub fn initialized(
        &mut self,
        generation: u64,
        operation: u64,
        negotiated_version: u32,
        steering: bool,
        now: u64,
    ) -> Result<Effect, Error> {
        self.startup_ack(generation, operation, Phase::Initializing, now)?;
        if negotiated_version != self.protocol {
            self.phase = Phase::Lost;
            return Err(Error::Unsupported);
        }
        let operation = self.operation()?;
        self.expected_operation = operation;
        self.phase = Phase::Creating;
        self.steering = steering;
        self.time = now;
        Ok(Effect::NewSession {
            generation,
            operation,
        })
    }
    pub fn created(
        &mut self,
        generation: u64,
        operation: u64,
        provider: Id,
        options: Vec<ConfigOption>,
        now: u64,
    ) -> Result<Effect, Error> {
        self.startup_ack(generation, operation, Phase::Creating, now)?;
        validate_options(&options)?;
        self.provider = Some(provider);
        self.time = now;
        self.config_requests.clear();
        if let Some(model) = &self.launch.model {
            self.config_requests.push_back((true, model.clone()));
        }
        if let Some(reasoning) = &self.launch.reasoning {
            self.config_requests.push_back((false, reasoning.clone()));
        }
        self.next_config(options)
    }
    fn next_config(&mut self, options: Vec<ConfigOption>) -> Result<Effect, Error> {
        let Some((model, wanted)) = self.config_requests.front().cloned() else {
            self.phase = Phase::Ready;
            return Ok(Effect::Ready {
                generation: self.generation,
                session: self.provider.clone().ok_or(Error::WrongPhase)?,
                steering: self.steering,
            });
        };
        let id = if model { "model" } else { "reasoning_effort" };
        let category = if model { "model" } else { "thought_level" };
        let config = options.iter().find(|c| {
            c.id.as_str() == id || c.category.as_ref().is_some_and(|s| s.as_str() == category)
        });
        let Some(config) = config else {
            self.phase = Phase::Lost;
            return Err(Error::Unsupported);
        };
        let claude_exact = model
            && self.launch.harness == Harness::Claude
            && config
                .category
                .as_ref()
                .is_some_and(|c| c.as_str() == "model");
        if !config.values.contains(&wanted) && !claude_exact {
            self.phase = Phase::Lost;
            return Err(Error::UnknownOption);
        }
        let config_id = config.id.clone();
        let operation = self.operation()?;
        self.expected_operation = operation;
        self.phase = Phase::Configuring;
        Ok(Effect::SetConfig {
            generation: self.generation,
            operation,
            session: self.provider.clone().ok_or(Error::WrongPhase)?,
            config: config_id,
            value: wanted,
        })
    }
    pub fn configured(
        &mut self,
        generation: u64,
        operation: u64,
        options: Vec<ConfigOption>,
        now: u64,
    ) -> Result<Effect, Error> {
        self.startup_ack(generation, operation, Phase::Configuring, now)?;
        validate_options(&options)?;
        self.config_requests.pop_front();
        self.time = now;
        self.next_config(options)
    }
    pub fn submit(
        &mut self,
        generation: u64,
        turn: Id,
        text: Text,
        now: u64,
    ) -> Result<Effect, Error> {
        self.check(generation, now)?;
        if self.phase != Phase::Ready {
            return Err(Error::WrongPhase);
        }
        if self.turn.is_some() {
            return Err(Error::Busy);
        }
        if self.seen_turns.contains(&turn) {
            return Err(Error::Duplicate);
        }
        if text.is_empty() {
            return Err(Error::InvalidInput);
        }
        if self.seen_turns.len() >= 256
            || self.messages.len() + 2 > 256
            || self.transcript_bytes + text.len() > MAX_TRANSCRIPT_BYTES
        {
            return Err(Error::LimitExceeded);
        }
        self.permissions.next_turn()?;
        self.tools.clear();
        self.thinking = true;
        self.last_stop_reason = None;
        self.seen_turns.insert(turn.clone());
        self.transcript_bytes += text.len();
        self.messages.push(Message {
            role: Role::Human,
            message_id: None,
            body: text.clone(),
        });
        self.messages.push(Message {
            role: Role::Assistant,
            message_id: None,
            body: Text::new("")?,
        });
        self.turn = Some(Turn {
            id: turn.clone(),
            cancelling: false,
            cancel_deadline: None,
            sequence: 0,
            steer: None,
            reply: self.messages.len() - 1,
            message_id: None,
        });
        self.time = now;
        Ok(Effect::Prompt {
            generation,
            session: self.provider.clone().ok_or(Error::WrongPhase)?,
            turn,
            text,
        })
    }
    fn turn_ref(&self, turn: &Id) -> Result<&Turn, Error> {
        let active = self.turn.as_ref().ok_or(Error::NoActiveTurn)?;
        if &active.id != turn {
            return Err(Error::StaleOperation);
        }
        Ok(active)
    }
    pub fn chunk(
        &mut self,
        generation: u64,
        turn: &Id,
        sequence: u64,
        message_id: Option<Id>,
        text: Text,
        now: u64,
    ) -> Result<(), Error> {
        self.check(generation, now)?;
        let active = self.turn_ref(turn)?;
        if active.cancelling {
            return Err(Error::Busy);
        }
        if sequence <= active.sequence {
            return Err(Error::Duplicate);
        }
        if active.sequence.checked_add(1) != Some(sequence) {
            return Err(Error::SequenceGap);
        }
        let boundary =
            message_id.is_some() && active.message_id.is_some() && message_id != active.message_id;
        if self.transcript_bytes + text.len() > MAX_TRANSCRIPT_BYTES
            || (boundary && self.messages.len() >= 256)
        {
            return Err(Error::LimitExceeded);
        }
        let active = self.turn.as_mut().expect("checked turn");
        if !text.is_empty() {
            if boundary {
                active.reply = self.messages.len();
                self.messages.push(Message {
                    role: Role::Assistant,
                    message_id: message_id.clone(),
                    body: Text::new("")?,
                });
            }
            if let Some(id) = message_id {
                active.message_id = Some(id.clone());
                self.messages[active.reply].message_id = Some(id);
            }
            self.messages[active.reply].body.append(&text);
            self.transcript_bytes += text.len();
        }
        active.sequence = sequence;
        self.thinking = false;
        self.time = now;
        Ok(())
    }
    pub fn tools(&self) -> impl Iterator<Item = &Tool> {
        self.tools.values()
    }
    pub fn thinking(&self) -> bool {
        self.thinking
    }
    pub fn last_stop_reason(&self) -> Option<&Id> {
        self.last_stop_reason.as_ref()
    }
    fn stream_header(
        &self,
        generation: u64,
        turn: &Id,
        sequence: u64,
        now: u64,
    ) -> Result<(), Error> {
        self.check(generation, now)?;
        let active = self.turn_ref(turn)?;
        if active.cancelling {
            return Err(Error::Busy);
        }
        if sequence <= active.sequence {
            return Err(Error::Duplicate);
        }
        if active.sequence.checked_add(1) != Some(sequence) {
            return Err(Error::SequenceGap);
        }
        Ok(())
    }
    pub fn tool_update(
        &mut self,
        generation: u64,
        turn: &Id,
        sequence: u64,
        tool: Tool,
        now: u64,
    ) -> Result<(), Error> {
        self.stream_header(generation, turn, sequence, now)?;
        if tool.title.len() > 4096 || (!self.tools.contains_key(&tool.id) && self.tools.len() >= 64)
        {
            return Err(Error::LimitExceeded);
        }
        self.tools.insert(tool.id.clone(), tool);
        self.turn.as_mut().expect("checked turn").sequence = sequence;
        self.time = now;
        Ok(())
    }
    /// Thought payload is intentionally absent: only a status signal crosses this boundary.
    pub fn thinking_update(
        &mut self,
        generation: u64,
        turn: &Id,
        sequence: u64,
        now: u64,
    ) -> Result<(), Error> {
        self.stream_header(generation, turn, sequence, now)?;
        self.thinking = true;
        self.turn.as_mut().expect("checked turn").sequence = sequence;
        self.time = now;
        Ok(())
    }
    pub fn steer(
        &mut self,
        generation: u64,
        turn: &Id,
        text: Text,
        now: u64,
    ) -> Result<Effect, Error> {
        self.check(generation, now)?;
        let active = self.turn_ref(turn)?;
        if !self.steering {
            return Err(Error::Unsupported);
        }
        if active.cancelling || active.steer.is_some() {
            return Err(Error::Busy);
        }
        if text.is_empty() {
            return Err(Error::InvalidInput);
        }
        let operation = self.operation()?;
        self.turn.as_mut().expect("checked turn").steer = Some(operation);
        self.time = now;
        Ok(Effect::Steer {
            generation,
            session: self.provider.clone().ok_or(Error::WrongPhase)?,
            turn: turn.clone(),
            operation,
            text,
        })
    }
    pub fn steered(
        &mut self,
        generation: u64,
        turn: &Id,
        operation: u64,
        now: u64,
    ) -> Result<(), Error> {
        self.check(generation, now)?;
        let active = self.turn_ref(turn)?;
        if active.steer != Some(operation) {
            return Err(Error::StaleOperation);
        }
        self.turn.as_mut().expect("checked turn").steer = None;
        self.time = now;
        Ok(())
    }
    pub fn permission(
        &mut self,
        generation: u64,
        turn: &Id,
        request: RequestId,
        options: Vec<Offered>,
        context: Option<Context>,
        now: u64,
    ) -> Result<Option<Effect>, Error> {
        self.check(generation, now)?;
        if self.turn_ref(turn)?.cancelling {
            return Err(Error::Busy);
        }
        let result = self
            .permissions
            .request(generation, request, options, context, now)?
            .map(Effect::Permission);
        self.time = now;
        Ok(result)
    }
    pub fn cancel_turn(
        &mut self,
        generation: u64,
        turn: &Id,
        now: u64,
    ) -> Result<Vec<Effect>, Error> {
        self.check(generation, now)?;
        let active = self.turn_ref(turn)?;
        if active.cancelling {
            return Err(Error::Busy);
        }
        let cancel_deadline = now.checked_add(5000).ok_or(Error::Exhausted)?;
        self.turn.as_mut().expect("checked turn").cancelling = true;
        self.turn.as_mut().expect("checked turn").cancel_deadline = Some(cancel_deadline);
        self.time = now;
        let mut effects = self
            .permissions
            .cancel_pending()
            .into_iter()
            .map(Effect::Permission)
            .collect::<Vec<_>>();
        effects.push(Effect::Cancel {
            generation,
            session: self.provider.clone().ok_or(Error::WrongPhase)?,
            turn: turn.clone(),
        });
        Ok(effects)
    }
    pub fn finished(&mut self, generation: u64, turn: &Id, now: u64) -> Result<Vec<Effect>, Error> {
        self.finished_reason(generation, turn, Id::new("end_turn")?, now)
    }
    pub fn finished_reason(
        &mut self,
        generation: u64,
        turn: &Id,
        stop_reason: Id,
        now: u64,
    ) -> Result<Vec<Effect>, Error> {
        self.check(generation, now)?;
        self.turn_ref(turn)?;
        self.last_stop_reason = Some(stop_reason);
        self.thinking = false;
        self.turn = None;
        self.time = now;
        let mut effects: Vec<_> = self
            .permissions
            .cancel_pending()
            .into_iter()
            .map(Effect::Permission)
            .collect();
        if self.presentation == Presentation::Pinned && !self.focused {
            effects.push(Effect::RequestAttention);
        }
        Ok(effects)
    }
    pub fn lost(&mut self, generation: u64, now: u64) -> Result<Vec<Effect>, Error> {
        self.check(generation, now)?;
        self.phase = Phase::Lost;
        self.thinking = false;
        self.turn = None;
        self.time = now;
        Ok(self
            .permissions
            .abort()
            .into_iter()
            .map(Effect::Permission)
            .collect())
    }
    /// Adapter first tears down/reaps prior child. Restart does not replay old transcript.
    pub fn restart(
        &mut self,
        launch: FrozenLaunch,
        prior_child_reaped: bool,
        now: u64,
    ) -> Result<Effect, Error> {
        self.check(self.generation, now)?;
        if self.phase != Phase::Lost || !prior_child_reaped {
            return Err(Error::WrongPhase);
        }
        if launch != self.launch {
            return Err(Error::InvalidConfig);
        }
        let generation = self.generation.checked_add(1).ok_or(Error::Exhausted)?;
        let deadline = now.checked_add(STARTUP_MS).ok_or(Error::Exhausted)?;
        let operation = self.operation()?;
        self.permissions.reset_generation(generation)?;
        self.generation = generation;
        self.launch = launch;
        self.deadline = deadline;
        self.time = now;
        self.phase = Phase::Initializing;
        self.expected_operation = operation;
        self.provider = None;
        self.steering = false;
        self.turn = None;
        self.messages.clear();
        self.tools.clear();
        self.thinking = false;
        self.last_stop_reason = None;
        self.transcript_bytes = 0;
        self.seen_turns.clear();
        self.config_requests.clear();
        Ok(Effect::Initialize {
            generation,
            operation,
            protocol_version: self.protocol,
        })
    }
    pub fn close(&mut self, now: u64) -> Result<Vec<Effect>, Error> {
        if now < self.time {
            return Err(Error::ClockWentBackwards);
        }
        if matches!(self.phase, Phase::Closing(_) | Phase::Closed) {
            return Ok(Vec::new());
        }
        let deadline = now.checked_add(350).ok_or(Error::Exhausted)?;
        let mut effects: Vec<_> = self
            .permissions
            .abort()
            .into_iter()
            .map(Effect::Permission)
            .collect();
        self.turn = None;
        self.messages.clear();
        self.tools.clear();
        self.thinking = false;
        self.last_stop_reason = None;
        self.transcript_bytes = 0;
        self.phase = Phase::Closing(CloseStage::Graceful);
        self.deadline = deadline;
        self.time = now;
        if let Some(session) = &self.provider {
            effects.push(Effect::CloseSession {
                generation: self.generation,
                session: session.clone(),
            });
        }
        Ok(effects)
    }
    pub fn child_exited(&mut self, generation: u64, now: u64) -> Result<Vec<Effect>, Error> {
        self.check(generation, now)?;
        if matches!(self.phase, Phase::Closing(_)) {
            self.phase = Phase::Closed;
            self.time = now;
            Ok(Vec::new())
        } else {
            self.lost(generation, now)
        }
    }
    pub fn tick(&mut self, now: u64) -> Result<Vec<Effect>, Error> {
        if now < self.time {
            return Err(Error::ClockWentBackwards);
        }
        self.time = now;
        if self
            .turn
            .as_ref()
            .is_some_and(|t| t.cancel_deadline.is_some_and(|deadline| now >= deadline))
        {
            return self.close(now);
        }
        match self.phase {
            Phase::Initializing | Phase::Creating | Phase::Configuring if now >= self.deadline => {
                self.close(now)
            }
            Phase::Closing(CloseStage::Graceful) if now >= self.deadline => {
                self.phase = Phase::Closing(CloseStage::Term);
                self.deadline = now.checked_add(500).ok_or(Error::Exhausted)?;
                Ok(vec![Effect::SignalTerm {
                    generation: self.generation,
                }])
            }
            Phase::Closing(CloseStage::Term) if now >= self.deadline => {
                self.phase = Phase::Closing(CloseStage::Kill);
                self.deadline = now.checked_add(150).ok_or(Error::Exhausted)?;
                Ok(vec![Effect::SignalKill {
                    generation: self.generation,
                }])
            }
            Phase::Closing(CloseStage::Kill) if now >= self.deadline => {
                Ok(vec![Effect::ReapOutstanding {
                    generation: self.generation,
                }])
            }
            _ => Ok(Vec::new()),
        }
    }
}
fn validate_options(options: &[ConfigOption]) -> Result<(), Error> {
    if options.len() > 64 || options.iter().any(|o| o.values.len() > 256) {
        return Err(Error::LimitExceeded);
    }
    if options.iter().map(|o| &o.id).collect::<BTreeSet<_>>().len() != options.len()
        || options
            .iter()
            .any(|o| o.values.iter().collect::<BTreeSet<_>>().len() != o.values.len())
    {
        return Err(Error::Duplicate);
    }
    Ok(())
}

/// Bounded ownership registry. Closing the transient overlay never closes pinned conversations.
pub struct Conversations {
    sessions: BTreeMap<u64, Session>,
    overlay: Option<u64>,
    next: u64,
}
impl Default for Conversations {
    fn default() -> Self {
        Self {
            sessions: BTreeMap::new(),
            overlay: None,
            next: 1,
        }
    }
}
impl Conversations {
    pub fn create_overlay(
        &mut self,
        launch: FrozenLaunch,
        protocol: u32,
        now: u64,
    ) -> Result<(u64, Effect), Error> {
        if self.overlay.is_some() {
            return Err(Error::Busy);
        }
        if self.sessions.len() >= 32 {
            return Err(Error::LimitExceeded);
        }
        let next = self.next.checked_add(1).ok_or(Error::Exhausted)?;
        let (session, effect) = Session::new(launch, protocol, now)?;
        let id = self.next;
        self.next = next;
        self.sessions.insert(id, session);
        self.overlay = Some(id);
        Ok((id, effect))
    }
    pub fn get(&self, id: u64) -> Result<&Session, Error> {
        self.sessions.get(&id).ok_or(Error::UnknownRequest)
    }
    pub fn get_mut(&mut self, id: u64) -> Result<&mut Session, Error> {
        self.sessions.get_mut(&id).ok_or(Error::UnknownRequest)
    }
    pub fn pin(&mut self, id: u64) -> Result<(), Error> {
        self.get_mut(id)?.pin()?;
        if self.overlay == Some(id) {
            self.overlay = None;
        }
        Ok(())
    }
    pub fn overlay(&self) -> Option<u64> {
        self.overlay
    }
    pub fn close_overlay(&mut self, now: u64) -> Result<Vec<Effect>, Error> {
        let Some(id) = self.overlay else {
            return Ok(Vec::new());
        };
        let effects = self.get_mut(id)?.close(now)?;
        self.overlay = None;
        Ok(effects)
    }
    pub fn remove_reaped(&mut self, id: u64) -> Result<(), Error> {
        if self.get(id)?.phase() != Phase::Closed {
            return Err(Error::Busy);
        }
        self.sessions.remove(&id);
        if self.overlay == Some(id) {
            self.overlay = None;
        }
        Ok(())
    }
}
