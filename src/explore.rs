//! Exploring one scope (system map or a component one level deeper), shared by
//! the web server and the full-project indexer.

use crate::agent::{self, AgentConfig};
use crate::cache::Cache;
use crate::map::{self, Map, Meta, Node};
use crate::repo::Repo;
use crate::{prompt, verify};
use std::path::PathBuf;
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::Notify;

pub struct Explorer {
    pub repo: Repo,
    pub cache: Cache,
    pub agent: AgentConfig,
    pub map_schema: PathBuf,
}

/// Everything needed to explore a scope below the system map.
pub struct Lineage {
    /// (name, summary) of the system and of every node down to the target.
    pub crumbs: Vec<(String, String)>,
    pub parent: Map,
    pub node: Node,
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

    /// Explore a scope with Codex, verify the answer and store it in the cache.
    pub async fn explore(
        &self,
        scope: &str,
        lineage: Option<&Lineage>,
        cancel: &Notify,
        mut progress: impl FnMut(String),
    ) -> Result<Map, String> {
        let (prompt, mut crumbs, paths) = match lineage {
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
        progress(format!("Exploring {} with codex (read-only sandbox)", crumbs.join(" › ")));
        let started = Instant::now();
        let out = agent::run::<Map>(&self.agent, &self.repo.root, &self.map_schema, &prompt, cancel, &mut progress).await?;

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
            model: self.agent.model.clone(),
            duration_ms: started.elapsed().as_millis() as u64,
            commands: out.commands,
            input_tokens: out.input_tokens,
            output_tokens: out.output_tokens,
        });
        self.cache.store(scope, &paths, manifest, commit, &map).map_err(|e| format!("saving map: {e}"))?;
        Ok(map)
    }
}
