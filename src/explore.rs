//! Exploring one scope (system map or a component one level deeper), shared by
//! the web server and the full-project indexer.
//!
//! Sessions mirror the map tree: the system map is explored in a new session,
//! each component in a fork of its parent's session, and a stale map is
//! refreshed inside the session that made it. Questions, example inputs and
//! walkthroughs fork the viewed map's session too, unless a separate ask engine or
//! model is set: then they start a session seeded with the atlas instead.

use crate::cache::{Cache, Freshness};
use crate::engine::{self, Engine, Usage};
use crate::map::{self, Map, Meta, Node};
use crate::repo::Repo;
use crate::{atlas, prompt, verify};
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::Notify;

pub struct Explorer {
    pub repo: Repo,
    pub cache: Cache,
    /// Makes and refreshes maps.
    pub engine: Engine,
    /// Answers questions and writes example inputs and walkthroughs when it differs
    /// from `engine` (another engine or model); `None` means `engine` does both.
    pub asker: Option<Engine>,
}

/// Everything needed to explore a scope below the system map.
pub struct Lineage {
    /// (name, summary) of the system and of every node down to the target.
    pub crumbs: Vec<(String, String)>,
    pub parent: Map,
    pub node: Node,
}

/// `a/b/c` → `a/b`; a top-level component's parent is the system map (`""`).
pub fn parent_scope(scope: &str) -> Option<&str> {
    if scope.is_empty() {
        None
    } else {
        Some(scope.rfind('/').map_or("", |i| &scope[..i]))
    }
}

impl Explorer {
    /// Resolve `a/b/c` through the cached maps above it (fresh or stale).
    pub fn lineage(&self, scope: &str) -> Result<Lineage, String> {
        let ids: Vec<&str> = scope.split('/').collect();
        if ids.iter().any(|id| id.is_empty()) {
            return Err(format!("invalid scope `{scope}`"));
        }
        let mut map = self.cache.load(&self.repo, "").ok_or("the system map has not been explored yet")?.map;
        let mut crumbs = vec![(map.title.clone(), map.summary.clone())];
        for (depth, id) in ids.iter().enumerate() {
            let node = map
                .nodes
                .iter()
                .find(|n| n.id == *id)
                .cloned()
                .ok_or_else(|| format!("no node `{id}` in the map above it"))?;
            crumbs.push((node.name.clone(), node.summary.clone()));
            if depth + 1 == ids.len() {
                if node.paths.is_empty() {
                    return Err(format!("`{}` has no repository paths to explore", node.name));
                }
                return Ok(Lineage { crumbs, parent: map, node });
            }
            let above = ids[..=depth].join("/");
            map = self
                .cache
                .load(&self.repo, &above)
                .ok_or_else(|| format!("`{}` has not been explored yet", node.name))?
                .map;
        }
        unreachable!("scope has at least one id")
    }

    pub fn session_name(&self, what: &str) -> String {
        format!("cxplr: {} › {what}", self.repo.name)
    }

    /// A new session that starts from `from`'s session when it has one (so it knows
    /// what that exploration read), otherwise a fresh session.
    pub async fn session_from(&self, from: Option<&str>, name: &str, progress: &mut impl FnMut(String)) -> Result<String, String> {
        self.fork_or_start(&self.engine, from, name, progress).await
    }

    /// `session_from` on `engine`, which must be the indexing engine's kind and model
    /// (it forks that engine's map sessions).
    async fn fork_or_start(&self, engine: &Engine, from: Option<&str>, name: &str, progress: &mut impl FnMut(String)) -> Result<String, String> {
        if let Some(parent) = from.and_then(|s| self.cache.session(s)) {
            match engine.fork(&parent, name).await {
                Ok(id) => {
                    self.record(&id, name);
                    progress(format!("Continuing from session {parent} (forked as {id})"));
                    return Ok(id);
                }
                // The parent session may have been deleted; start over without it.
                Err(e) => progress(format!("! cannot fork session {parent}: {e}; starting a new session")),
            }
        }
        let id = engine.start_session(name).await?;
        self.record(&id, name);
        progress(format!("New session {id}"));
        Ok(id)
    }

    fn record(&self, id: &str, name: &str) {
        // Losing the registry entry only means `--forget-sessions` misses it.
        let _ = self.cache.record_session(self.engine.kind(), id, name);
    }

    /// The engine for questions, example inputs and walkthroughs.
    pub fn ask_engine(&self) -> &Engine {
        self.asker.as_ref().unwrap_or(&self.engine)
    }

    /// The ask engine cannot fork map sessions: it is another engine, or another
    /// model that would read that history cold. (Another effort alone keeps the
    /// engine and model, so forking still carries over what the session read.)
    pub fn ask_seeded(&self) -> bool {
        self.asker.as_ref().is_some_and(|a| a.kind() != self.engine.kind() || a.model() != self.engine.model())
    }

    /// A session for a question, example inputs or a walkthrough about `scope`, and
    /// the context to put in front of its first prompt. Normally this forks the
    /// scope's map session, which has already read that code. A seeded ask engine
    /// (see `ask_seeded`) starts a new session with the atlas pages down to `scope`.
    pub async fn ask_session(&self, scope: &str, name: &str, progress: &mut impl FnMut(String)) -> Result<(String, Option<String>), String> {
        let asker = self.ask_engine();
        if !self.ask_seeded() {
            return Ok((self.fork_or_start(asker, Some(scope), name, progress).await?, None));
        }
        let id = asker.start_session(name).await?;
        let _ = self.cache.record_session(asker.kind(), &id, name);
        let context = atlas::context(&self.cache, scope);
        let model = asker.model().map_or(String::new(), |m| format!(", {m}"));
        let seed = if context.is_some() { "seeded with the atlas" } else { "no atlas pages to seed it with" };
        progress(format!("New {}{model} session {id}, {seed}", asker.kind().id()));
        Ok((id, context))
    }

    /// Explore a scope, verify the answer and store it in the cache.
    pub async fn explore(
        &self,
        scope: &str,
        lineage: Option<&Lineage>,
        cancel: &Notify,
        mut progress: impl FnMut(String),
        mut usage: impl FnMut(Usage),
    ) -> Result<Map, String> {
        let (full_prompt, mut crumbs, paths) = match lineage {
            None => (prompt::system(&self.repo.name), vec![self.repo.name.clone()], vec![".".to_owned()]),
            Some(l) => (
                prompt::component(&l.crumbs, &l.parent, &l.node),
                l.crumbs.iter().map(|(name, _)| name.clone()).collect(),
                l.node.paths.clone(),
            ),
        };
        // The map describes the files as they were when the exploration started.
        let manifest = self.repo.manifest(&paths);
        let commit = self.repo.head();
        let label = crumbs.join(" › ");
        // `cxplr: <repo> › system map` / `cxplr: <repo> › Component › Sub`
        let name = self.session_name(&if crumbs.len() > 1 { crumbs[1..].join(" › ") } else { "system map".into() });

        // A stale map with a session is updated in that session; everything else
        // is explored in a fork of the parent's session.
        let stale = self.cache.load(&self.repo, scope).and_then(|c| match c.freshness {
            Freshness::Stale { changed, total, .. } => Some((changed, total)),
            Freshness::Fresh => None,
        });
        let (session, prompt, refreshed) = match (stale, self.cache.session(scope)) {
            (Some((changed, total)), Some(session)) => {
                progress(format!("Refreshing {label} in its session {session} ({total} files changed)"));
                (session, prompt::refresh(&changed, total), true)
            }
            _ => {
                progress(format!("Exploring {label} with {} (read-only)", self.engine.kind().id()));
                let session = self.session_from(parent_scope(scope), &name, &mut progress).await?;
                (session, full_prompt.clone(), false)
            }
        };

        let started = Instant::now();
        let (session, refreshed, out) = match self.engine.turn::<Map>(&session, &prompt, &map::schema(), cancel, &mut progress, &mut usage).await {
            Ok(out) => (session, refreshed, out),
            // The map's session no longer exists (deleted): explore from scratch instead.
            Err(e) if refreshed && e.starts_with(engine::SESSION_GONE) => {
                progress(format!("! session {session} is gone ({e}); exploring from scratch"));
                let fresh = self.session_from(parent_scope(scope), &name, &mut progress).await?;
                let out = self.engine.turn::<Map>(&fresh, &full_prompt, &map::schema(), cancel, &mut progress, &mut usage).await?;
                (fresh, false, out)
            }
            Err(e) => return Err(e),
        };

        progress("Verifying citations against the repository".into());
        let mut map = out.answer;
        map.diagnostics.clear();
        map::normalize(&mut map);
        verify::verify(&self.repo.root, &mut map);
        if lineage.is_none() && !map.title.trim().is_empty() {
            crumbs[0] = map.title.clone();
        }
        map.meta = Some(Meta {
            scope: scope.to_owned(),
            crumbs,
            generated_at: SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs()),
            commit: commit.clone(),
            dirty: self.repo.is_dirty(),
            model: self.engine.model().map(str::to_owned),
            effort: self.engine.effort().map(str::to_owned),
            duration_ms: started.elapsed().as_millis() as u64,
            commands: out.commands,
            input_tokens: out.input_tokens,
            output_tokens: out.output_tokens,
            cached_input_tokens: out.cached_input_tokens,
            session: Some(session.clone()),
            engine: Some(self.engine.kind().id().to_owned()),
            refreshed,
        });
        self.cache
            .store(scope, &paths, manifest, commit, Some(session), &map)
            .map_err(|e| format!("saving map: {e}"))?;
        Ok(map)
    }
}

#[cfg(test)]
mod tests {
    use super::parent_scope;

    #[test]
    fn sessions_fork_from_the_enclosing_map() {
        assert_eq!(parent_scope(""), None);
        assert_eq!(parent_scope("hermes"), Some(""));
        assert_eq!(parent_scope("hermes/runtime/x"), Some("hermes/runtime"));
    }
}
