//! Closed application frames over the shared authenticated presenter transport.
use crate::{Command, View};
use modal_client::presenter::{Auth, Counter};
use serde::{Deserialize, Serialize};
use std::{
    io,
    path::Path,
    time::{Duration, Instant},
};
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub version: u8,
    pub request: Counter,
    pub auth: Auth,
    pub command: Operation,
}
#[derive(Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Operation {
    Read {
        cursor: usize,
        revision: Option<Counter>,
    },
    Apply {
        revision: Counter,
        command: Command,
    },
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reply {
    pub version: u8,
    pub request: Counter,
    pub auth: Auth,
    pub revision: Counter,
    pub result: ResultBody,
}
#[derive(Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum ResultBody {
    Changed,
    Page {
        view: Box<View>,
        next: Option<usize>,
        metadata_truncated: bool,
    },
    Applied {
        closed: bool,
    },
}
pub fn decode(bytes: &[u8]) -> io::Result<Request> {
    if bytes.len() > 4096 {
        return Err(io::Error::other("frame limit"));
    }
    let r: Request = serde_json::from_slice(bytes)?;
    if r.version != 1 {
        return Err(io::Error::other("version"));
    }
    r.auth.validate().map_err(io::Error::other)?;
    Ok(r)
}
fn truncate(text: &mut String) -> bool {
    if text.len() <= 384 {
        return false;
    }
    let mut end = 384;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    text.push('…');
    true
}
pub fn page(mut view: View, cursor: usize) -> io::Result<(View, Option<usize>, bool)> {
    if cursor > view.rows.len() || cursor > 100 {
        return Err(io::Error::other("cursor"));
    }
    let next = (cursor + 1 < view.rows.len()).then_some(cursor + 1);
    view.rows = view.rows.into_iter().skip(cursor).take(1).collect();
    let mut cut = false;
    for row in &mut view.rows {
        cut |= truncate(&mut row.title);
        cut |= truncate(&mut row.class);
        cut |= truncate(&mut row.workspace);
    }
    Ok((view, next, cut))
}
pub struct Model {
    pub view: View,
    pub metadata_truncated: bool,
}
pub struct Link {
    client: modal_client::data::Client,
    auth: Auth,
    request: u64,
    revision: Option<Counter>,
    failed: bool,
}
impl Link {
    pub fn connect(path: &Path, peer: desktop_io::ProcessIdentity, auth: Auth) -> io::Result<Self> {
        auth.validate().map_err(io::Error::other)?;
        Ok(Self {
            client: modal_client::data::Client::connect(path, peer)?,
            auth,
            request: 0,
            revision: None,
            failed: false,
        })
    }
    fn exchange(&mut self, command: Operation, deadline: Instant) -> io::Result<Reply> {
        if self.failed {
            return Err(io::Error::other("failed presenter link"));
        }
        let result = (|| {
            self.request = self
                .request
                .checked_add(1)
                .ok_or_else(|| io::Error::other("request exhausted"))?;
            let request = Counter::new(self.request).unwrap();
            let frame = Request {
                version: 1,
                request,
                auth: self.auth.clone(),
                command,
            };
            let bytes = self.client.exchange(
                &serde_json::to_vec(&frame)?,
                deadline.min(Instant::now() + Duration::from_millis(500)),
            )?;
            let reply: Reply = serde_json::from_slice(&bytes)?;
            if reply.version != 1 || reply.request != request || reply.auth != self.auth {
                return Err(io::Error::other("reply context"));
            }
            Ok(reply)
        })();
        if result.is_err() {
            self.failed = true;
            self.client.invalidate();
        }
        result
    }
    pub fn model(&mut self) -> io::Result<Model> {
        let result = (|| {
            let deadline = Instant::now() + Duration::from_secs(2);
            let mut minimum = self.revision;
            for _ in 0..3 {
                self.revision = None;
                let mut cursor = 0;
                let mut model: Option<View> = None;
                let mut ids = std::collections::BTreeSet::new();
                let mut cut = false;
                loop {
                    let reply = self.exchange(
                        Operation::Read {
                            cursor,
                            revision: self.revision,
                        },
                        deadline,
                    )?;
                    if minimum.is_some_and(|r| reply.revision.get() < r.get()) {
                        return Err(io::Error::other("model revision rollback"));
                    }
                    if matches!(reply.result, ResultBody::Changed) {
                        if !self
                            .revision
                            .is_some_and(|r| reply.revision.get() > r.get())
                        {
                            return Err(io::Error::other("invalid changed revision"));
                        }
                        minimum = Some(reply.revision);
                        model = None;
                        break;
                    }
                    if self.revision.is_some_and(|r| r != reply.revision) {
                        return Err(io::Error::other("model changed"));
                    }
                    self.revision = Some(reply.revision);
                    let ResultBody::Page {
                        view,
                        next,
                        metadata_truncated,
                    } = reply.result
                    else {
                        return Err(io::Error::other("page expected"));
                    };
                    let mut view = *view;
                    if !view.open
                        || view.stale
                        || view.rows.len() > 1
                        || view.total > 4096
                        || view.generation == 0
                        || view.revision == 0
                        || !view.settings.volume.is_finite()
                        || !(0.0..=1.0).contains(&view.settings.volume)
                    {
                        return Err(io::Error::other("invalid view"));
                    }
                    let rows = std::mem::take(&mut view.rows);
                    for row in &rows {
                        if row.id == 0
                            || row.title.len() > 387
                            || row.class.len() > 387
                            || row.workspace.len() > 387
                            || !ids.insert(row.id)
                        {
                            return Err(io::Error::other("invalid row"));
                        }
                    }
                    if let Some(prior) = &model {
                        let mut old = prior.clone();
                        old.rows.clear();
                        if old != view {
                            return Err(io::Error::other("mixed model"));
                        }
                    } else {
                        model = Some(view);
                    }
                    let model = model.as_mut().unwrap();
                    model.rows.extend(rows);
                    cut |= metadata_truncated;
                    if model.rows.len() > 100 {
                        return Err(io::Error::other("row bound"));
                    }
                    match next {
                        Some(n) if n == model.rows.len() && n > cursor && n < 100 => cursor = n,
                        None => break,
                        _ => return Err(io::Error::other("pagination")),
                    }
                }
                if let Some(view) = model {
                    return Ok(Model {
                        view,
                        metadata_truncated: cut,
                    });
                }
            }
            Err(io::Error::other("model changing; restart budget exhausted"))
        })();
        if result.is_err() {
            self.failed = true;
            self.client.invalidate();
        }
        result
    }
    pub fn apply(&mut self, command: Command) -> io::Result<bool> {
        let revision = self
            .revision
            .ok_or_else(|| io::Error::other("model missing"))?;
        let result = (|| {
            let reply = self.exchange(
                Operation::Apply { revision, command },
                Instant::now() + Duration::from_millis(500),
            )?;
            if reply.revision.get()
                != revision
                    .get()
                    .checked_add(1)
                    .ok_or_else(|| io::Error::other("revision exhausted"))?
            {
                return Err(io::Error::other("revision mismatch"));
            }
            let ResultBody::Applied { closed } = reply.result else {
                return Err(io::Error::other("application reply"));
            };
            self.revision = Some(reply.revision);
            Ok(closed)
        })();
        if result.is_err() {
            self.failed = true;
            self.client.invalidate();
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn long_unicode_metadata_is_bounded_and_flagged() {
        let mut runtime = crate::Runtime::fixture().unwrap();
        runtime.execute(Command::Open, 5).unwrap();
        let mut view = runtime.view();
        view.rows[0].title = "👩‍💻".repeat(1000);
        let (page, next, cut) = page(view, 0).unwrap();
        assert!(cut);
        assert!(page.rows[0].title.len() <= 387);
        assert!(page.rows[0].title.ends_with('…'));
        assert_eq!(next, Some(1));
        assert!(serde_json::to_vec(&page).unwrap().len() < 3000);
    }
    #[test]
    fn shared_counter_and_closed_request_reject_forgery() {
        assert!(serde_json::from_str::<Counter>("\"01\"").is_err());
        assert!(serde_json::from_str::<Counter>("\"18446744073709551616\"").is_err());
        assert!(decode(br#"{"version":1,"version":1}"#).is_err());
    }
}
