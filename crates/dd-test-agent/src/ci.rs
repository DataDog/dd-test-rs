// Copyright 2026-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

use crate::read_cache::ReadCache;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::Path;

pub type Tags = BTreeMap<String, String>;

pub trait Env {
    fn get(&self, key: &str) -> Option<String>;
    fn vars(&self) -> Vec<(String, String)>;
}

pub struct ProcessEnv;

impl Env for ProcessEnv {
    fn get(&self, key: &str) -> Option<String> {
        std::env::var(key).ok()
    }

    fn vars(&self) -> Vec<(String, String)> {
        std::env::vars().collect()
    }
}

struct Reader<'a>(&'a dyn Env);

impl Reader<'_> {
    fn get(&self, key: &str) -> String {
        self.0.get(key).unwrap_or_default()
    }

    fn first(&self, keys: &[&str]) -> String {
        keys.iter()
            .map(|k| self.get(k))
            .find(|v| !v.is_empty())
            .unwrap_or_default()
    }

    fn env_json(&self, keys: &[&str]) -> String {
        let map: BTreeMap<&str, String> = keys
            .iter()
            .filter_map(|k| {
                let v = self.get(k);
                (!v.is_empty()).then_some((*k, v))
            })
            .collect();
        serde_json::to_string(&map).unwrap_or_default()
    }
}

type Extractor = fn(&Reader<'_>, &mut Tags);

const PROVIDERS: &[(&str, Extractor)] = &[
    ("APPVEYOR", appveyor),
    ("TF_BUILD", azure_pipelines),
    ("BITBUCKET_COMMIT", bitbucket),
    ("BUDDY", buddy),
    ("BUILDKITE", buildkite),
    ("CIRCLECI", circleci),
    ("GITHUB_SHA", github_actions),
    ("GITLAB_CI", gitlab),
    ("JENKINS_URL", jenkins),
    ("TEAMCITY_VERSION", teamcity),
    ("TRAVIS", travis),
    ("BITRISE_BUILD_SLUG", bitrise),
    ("CF_BUILD_ID", codefresh),
    ("CODEBUILD_INITIATOR", aws_codepipeline),
    ("DRONE", drone),
];

const USER_SUPPLIED: &[(&str, &str)] = &[
    ("git.branch", "DD_GIT_BRANCH"),
    ("git.tag", "DD_GIT_TAG"),
    ("git.repository_url", "DD_GIT_REPOSITORY_URL"),
    ("git.commit.sha", "DD_GIT_COMMIT_SHA"),
    ("git.commit.message", "DD_GIT_COMMIT_MESSAGE"),
    ("git.commit.author.name", "DD_GIT_COMMIT_AUTHOR_NAME"),
    ("git.commit.author.email", "DD_GIT_COMMIT_AUTHOR_EMAIL"),
    ("git.commit.author.date", "DD_GIT_COMMIT_AUTHOR_DATE"),
    ("git.commit.committer.name", "DD_GIT_COMMIT_COMMITTER_NAME"),
    ("git.commit.committer.email", "DD_GIT_COMMIT_COMMITTER_EMAIL"),
    ("git.commit.committer.date", "DD_GIT_COMMIT_COMMITTER_DATE"),
    ("git.pull_request.base_branch", "DD_GIT_PULL_REQUEST_BASE_BRANCH"),
    (
        "git.pull_request.base_branch_sha",
        "DD_GIT_PULL_REQUEST_BASE_BRANCH_SHA",
    ),
];

/// CI provider tags with user-supplied git overrides, normalized.
pub fn provider_tags(env: &dyn Env) -> Tags {
    let reader = Reader(env);
    let mut tags = Tags::new();
    for (key, extract) in PROVIDERS {
        if env.get(key).is_some() {
            tags.clear();
            extract(&reader, &mut tags);
        }
    }
    apply_user_supplied(env, &mut tags);
    normalize(&mut tags);
    if let Some(path) = tags.get("ci.workspace_path").cloned() {
        tags.insert("ci.workspace_path".into(), expand_home(&path, env));
    }
    tags.retain(|_, v| !v.is_empty());
    tags
}

pub fn apply_user_supplied(env: &dyn Env, tags: &mut Tags) {
    for (tag, var) in USER_SUPPLIED {
        if let Some(value) = env.get(var).filter(|v| !v.is_empty()) {
            tags.insert(tag.to_string(), value);
        }
    }
}

pub fn normalize(tags: &mut Tags) {
    if let Some(branch) = tags.get("git.branch").filter(|b| !b.is_empty()).cloned() {
        if branch.contains("refs/tags") || branch.contains("origin/tags") || branch.contains("refs/heads/tags") {
            tags.insert("git.tag".into(), normalize_ref(&branch));
        }
        tags.insert("git.branch".into(), normalize_ref(&branch));
    }
    for key in ["git.tag", "git.pull_request.base_branch"] {
        if let Some(value) = tags.get_mut(key) {
            *value = normalize_ref(value);
        }
    }
    for key in ["git.repository_url", "ci.pipeline.url", "ci.job.url", "_dd.ci.env_vars"] {
        if let Some(value) = tags.get_mut(key) {
            *value = filter_sensitive_info(value);
        }
    }
}

pub fn normalize_ref(name: &str) -> String {
    let mut name = name;
    for prefix in ["refs/heads/", "refs/", "origin/", "tags/"] {
        if let Some(rest) = name.strip_prefix(prefix) {
            name = rest;
        }
    }
    name.to_string()
}

/// Removes `user:password@` credentials after `http(s)://` and `ss(h)://` schemes.
pub fn filter_sensitive_info(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut rest = value;
    while let Some((start, scheme_len)) = find_scheme(rest) {
        let after = start + scheme_len;
        out.push_str(&rest[..after]);
        let authority_end = rest[after..].find('/').map(|i| after + i).unwrap_or(rest.len());
        match rest[after..authority_end].rfind('@') {
            Some(at) => rest = &rest[after + at + 1..],
            None => rest = &rest[after..],
        }
    }
    out.push_str(rest);
    out
}

fn find_scheme(value: &str) -> Option<(usize, usize)> {
    ["https://", "http://", "ssh://", "ss://"]
        .iter()
        .filter_map(|scheme| value.find(scheme).map(|i| (i, scheme.len())))
        .min_by_key(|(i, len)| (*i, std::cmp::Reverse(*len)))
}

fn expand_home(path: &str, env: &dyn Env) -> String {
    let Some(rest) = path.strip_prefix('~') else {
        return path.to_string();
    };
    if !(rest.is_empty() || rest.starts_with('/') || rest.starts_with('\\')) {
        return path.to_string();
    }
    match env.get("HOME").filter(|h| !h.is_empty()) {
        Some(home) => format!("{}{}", home.trim_end_matches('/'), rest),
        None => path.to_string(),
    }
}

fn put(tags: &mut Tags, key: &str, value: String) {
    tags.insert(key.to_string(), value);
}

fn split_branch_or_tag(value: String) -> (String, String) {
    if value.contains("tags/") {
        (String::new(), value)
    } else {
        (value, String::new())
    }
}

fn appveyor(e: &Reader<'_>, t: &mut Tags) {
    let url = format!(
        "https://ci.appveyor.com/project/{}/builds/{}",
        e.get("APPVEYOR_REPO_NAME"),
        e.get("APPVEYOR_BUILD_ID")
    );
    put(t, "ci.provider.name", "appveyor".into());
    let repo = if e.get("APPVEYOR_REPO_PROVIDER") == "github" {
        format!("https://github.com/{}.git", e.get("APPVEYOR_REPO_NAME"))
    } else {
        e.get("APPVEYOR_REPO_NAME")
    };
    put(t, "git.repository_url", repo);
    put(t, "git.commit.sha", e.get("APPVEYOR_REPO_COMMIT"));
    put(
        t,
        "git.branch",
        e.first(&["APPVEYOR_PULL_REQUEST_HEAD_REPO_BRANCH", "APPVEYOR_REPO_BRANCH"]),
    );
    put(t, "git.tag", e.get("APPVEYOR_REPO_TAG_NAME"));
    put(t, "ci.workspace_path", e.get("APPVEYOR_BUILD_FOLDER"));
    put(t, "ci.pipeline.id", e.get("APPVEYOR_BUILD_ID"));
    put(t, "ci.pipeline.name", e.get("APPVEYOR_REPO_NAME"));
    put(t, "ci.pipeline.number", e.get("APPVEYOR_BUILD_NUMBER"));
    put(t, "ci.pipeline.url", url.clone());
    put(t, "ci.job.url", url);
    put(
        t,
        "git.commit.message",
        format!(
            "{}\n{}",
            e.get("APPVEYOR_REPO_COMMIT_MESSAGE"),
            e.get("APPVEYOR_REPO_COMMIT_MESSAGE_EXTENDED")
        ),
    );
    put(t, "git.commit.author.name", e.get("APPVEYOR_REPO_COMMIT_AUTHOR"));
    put(t, "git.commit.author.email", e.get("APPVEYOR_REPO_COMMIT_AUTHOR_EMAIL"));
    put(t, "git.pull_request.base_branch", e.get("APPVEYOR_REPO_BRANCH"));
    put(t, "git.commit.head.sha", e.get("APPVEYOR_PULL_REQUEST_HEAD_COMMIT"));
    put(t, "pr.number", e.get("APPVEYOR_PULL_REQUEST_NUMBER"));
}

fn azure_pipelines(e: &Reader<'_>, t: &mut Tags) {
    let base = format!(
        "{}{}/_build/results?buildId={}",
        e.get("SYSTEM_TEAMFOUNDATIONSERVERURI"),
        e.get("SYSTEM_TEAMPROJECTID"),
        e.get("BUILD_BUILDID")
    );
    let job_url = format!(
        "{base}&view=logs&j={}&t={}",
        e.get("SYSTEM_JOBID"),
        e.get("SYSTEM_TASKINSTANCEID")
    );
    let (branch, tag) = split_branch_or_tag(e.first(&[
        "SYSTEM_PULLREQUEST_SOURCEBRANCH",
        "BUILD_SOURCEBRANCH",
        "BUILD_SOURCEBRANCHNAME",
    ]));
    put(t, "ci.provider.name", "azurepipelines".into());
    put(t, "ci.workspace_path", e.get("BUILD_SOURCESDIRECTORY"));
    put(t, "ci.pipeline.id", e.get("BUILD_BUILDID"));
    put(t, "ci.pipeline.name", e.get("BUILD_DEFINITIONNAME"));
    put(t, "ci.pipeline.number", e.get("BUILD_BUILDID"));
    put(t, "ci.pipeline.url", base);
    put(t, "ci.stage.name", e.get("SYSTEM_STAGEDISPLAYNAME"));
    put(t, "ci.job.id", e.get("SYSTEM_JOBID"));
    put(t, "ci.job.name", e.get("SYSTEM_JOBDISPLAYNAME"));
    put(t, "ci.job.url", job_url);
    put(
        t,
        "git.repository_url",
        e.first(&["SYSTEM_PULLREQUEST_SOURCEREPOSITORYURI", "BUILD_REPOSITORY_URI"]),
    );
    put(
        t,
        "git.commit.sha",
        e.first(&["SYSTEM_PULLREQUEST_SOURCECOMMITID", "BUILD_SOURCEVERSION"]),
    );
    put(t, "git.branch", branch);
    put(t, "git.tag", tag);
    put(t, "git.commit.message", e.get("BUILD_SOURCEVERSIONMESSAGE"));
    put(t, "git.commit.author.name", e.get("BUILD_REQUESTEDFORID"));
    put(t, "git.commit.author.email", e.get("BUILD_REQUESTEDFOREMAIL"));
    put(
        t,
        "_dd.ci.env_vars",
        e.env_json(&["SYSTEM_TEAMPROJECTID", "BUILD_BUILDID", "SYSTEM_JOBID"]),
    );
    put(
        t,
        "git.pull_request.base_branch",
        e.get("SYSTEM_PULLREQUEST_TARGETBRANCH"),
    );
    put(t, "pr.number", e.get("SYSTEM_PULLREQUEST_PULLREQUESTNUMBER"));
}

fn bitrise(e: &Reader<'_>, t: &mut Tags) {
    put(t, "ci.provider.name", "bitrise".into());
    put(t, "git.repository_url", e.get("GIT_REPOSITORY_URL"));
    put(
        t,
        "git.commit.sha",
        e.first(&["BITRISE_GIT_COMMIT", "GIT_CLONE_COMMIT_HASH"]),
    );
    put(
        t,
        "git.branch",
        e.first(&["BITRISEIO_PULL_REQUEST_HEAD_BRANCH", "BITRISE_GIT_BRANCH"]),
    );
    put(t, "git.tag", e.get("BITRISE_GIT_TAG"));
    put(t, "ci.workspace_path", e.get("BITRISE_SOURCE_DIR"));
    put(t, "ci.pipeline.id", e.get("BITRISE_BUILD_SLUG"));
    put(t, "ci.pipeline.name", e.get("BITRISE_TRIGGERED_WORKFLOW_ID"));
    put(t, "ci.pipeline.number", e.get("BITRISE_BUILD_NUMBER"));
    put(t, "ci.pipeline.url", e.get("BITRISE_BUILD_URL"));
    put(t, "git.commit.message", e.get("BITRISE_GIT_MESSAGE"));
    put(t, "git.pull_request.base_branch", e.get("BITRISEIO_GIT_BRANCH_DEST"));
    put(t, "pr.number", e.get("BITRISE_PULL_REQUEST"));
}

fn bitbucket(e: &Reader<'_>, t: &mut Tags) {
    let url = format!(
        "https://bitbucket.org/{}/addon/pipelines/home#!/results/{}",
        e.get("BITBUCKET_REPO_FULL_NAME"),
        e.get("BITBUCKET_BUILD_NUMBER")
    );
    put(t, "ci.provider.name", "bitbucket".into());
    put(
        t,
        "git.repository_url",
        e.first(&["BITBUCKET_GIT_SSH_ORIGIN", "BITBUCKET_GIT_HTTP_ORIGIN"]),
    );
    put(t, "git.commit.sha", e.get("BITBUCKET_COMMIT"));
    put(t, "git.branch", e.get("BITBUCKET_BRANCH"));
    put(t, "git.tag", e.get("BITBUCKET_TAG"));
    put(t, "ci.workspace_path", e.get("BITBUCKET_CLONE_DIR"));
    put(
        t,
        "ci.pipeline.id",
        e.get("BITBUCKET_PIPELINE_UUID")
            .trim_matches(|c| c == '{' || c == '}')
            .to_string(),
    );
    put(t, "ci.pipeline.number", e.get("BITBUCKET_BUILD_NUMBER"));
    put(t, "ci.pipeline.name", e.get("BITBUCKET_REPO_FULL_NAME"));
    put(t, "ci.pipeline.url", url.clone());
    put(t, "ci.job.url", url);
    put(
        t,
        "git.pull_request.base_branch",
        e.get("BITBUCKET_PR_DESTINATION_BRANCH"),
    );
    put(t, "pr.number", e.get("BITBUCKET_PR_ID"));
}

fn buddy(e: &Reader<'_>, t: &mut Tags) {
    put(t, "ci.provider.name", "buddy".into());
    put(
        t,
        "ci.pipeline.id",
        format!("{}/{}", e.get("BUDDY_PIPELINE_ID"), e.get("BUDDY_EXECUTION_ID")),
    );
    put(t, "ci.pipeline.name", e.get("BUDDY_PIPELINE_NAME"));
    put(t, "ci.pipeline.number", e.get("BUDDY_EXECUTION_ID"));
    put(t, "ci.pipeline.url", e.get("BUDDY_EXECUTION_URL"));
    put(t, "git.commit.sha", e.get("BUDDY_EXECUTION_REVISION"));
    put(t, "git.repository_url", e.get("BUDDY_SCM_URL"));
    put(t, "git.branch", e.get("BUDDY_EXECUTION_BRANCH"));
    put(t, "git.tag", e.get("BUDDY_EXECUTION_TAG"));
    put(t, "git.commit.message", e.get("BUDDY_EXECUTION_REVISION_MESSAGE"));
    put(
        t,
        "git.commit.committer.name",
        e.get("BUDDY_EXECUTION_REVISION_COMMITTER_NAME"),
    );
    put(
        t,
        "git.commit.committer.email",
        e.get("BUDDY_EXECUTION_REVISION_COMMITTER_EMAIL"),
    );
    put(t, "git.pull_request.base_branch", e.get("BUDDY_RUN_PR_BASE_BRANCH"));
    put(t, "pr.number", e.get("BUDDY_RUN_PR_NO"));
}

fn buildkite(e: &Reader<'_>, t: &mut Tags) {
    put(t, "git.branch", e.get("BUILDKITE_BRANCH"));
    put(t, "git.commit.sha", e.get("BUILDKITE_COMMIT"));
    put(t, "git.repository_url", e.get("BUILDKITE_REPO"));
    put(t, "git.tag", e.get("BUILDKITE_TAG"));
    put(t, "ci.pipeline.id", e.get("BUILDKITE_BUILD_ID"));
    put(t, "ci.pipeline.name", e.get("BUILDKITE_PIPELINE_SLUG"));
    put(t, "ci.pipeline.display_name", e.get("BUILDKITE_PIPELINE_NAME"));
    put(t, "ci.pipeline.number", e.get("BUILDKITE_BUILD_NUMBER"));
    put(t, "ci.pipeline.url", e.get("BUILDKITE_BUILD_URL"));
    put(t, "ci.job.id", e.get("BUILDKITE_JOB_ID"));
    put(
        t,
        "ci.job.url",
        format!("{}#{}", e.get("BUILDKITE_BUILD_URL"), e.get("BUILDKITE_JOB_ID")),
    );
    put(t, "ci.provider.name", "buildkite".into());
    put(t, "ci.workspace_path", e.get("BUILDKITE_BUILD_CHECKOUT_PATH"));
    put(t, "git.commit.message", e.get("BUILDKITE_MESSAGE"));
    put(t, "git.commit.author.name", e.get("BUILDKITE_BUILD_AUTHOR"));
    put(t, "git.commit.author.email", e.get("BUILDKITE_BUILD_AUTHOR_EMAIL"));
    put(t, "ci.node.name", e.get("BUILDKITE_AGENT_ID"));
    put(
        t,
        "_dd.ci.env_vars",
        e.env_json(&["BUILDKITE_BUILD_ID", "BUILDKITE_JOB_ID"]),
    );
    let mut labels: Vec<String> =
        e.0.vars()
            .into_iter()
            .filter_map(|(k, v)| {
                k.strip_prefix("BUILDKITE_AGENT_META_DATA_")
                    .map(|rest| format!("{rest}:{v}").to_lowercase())
            })
            .collect();
    if !labels.is_empty() {
        labels.sort_by(|a, b| b.cmp(a));
        put(t, "ci.node.labels", serde_json::to_string(&labels).unwrap_or_default());
    }
    put(
        t,
        "git.pull_request.base_branch",
        e.get("BUILDKITE_PULL_REQUEST_BASE_BRANCH"),
    );
    put(t, "pr.number", e.get("BUILDKITE_PULL_REQUEST"));
}

fn circleci(e: &Reader<'_>, t: &mut Tags) {
    put(t, "ci.provider.name", "circleci".into());
    put(t, "git.repository_url", e.get("CIRCLE_REPOSITORY_URL"));
    put(t, "git.commit.sha", e.get("CIRCLE_SHA1"));
    put(t, "git.tag", e.get("CIRCLE_TAG"));
    put(t, "git.branch", e.get("CIRCLE_BRANCH"));
    put(t, "ci.workspace_path", e.get("CIRCLE_WORKING_DIRECTORY"));
    put(t, "ci.pipeline.id", e.get("CIRCLE_WORKFLOW_ID"));
    put(t, "ci.pipeline.name", e.get("CIRCLE_PROJECT_REPONAME"));
    put(t, "ci.pipeline.number", e.get("CIRCLE_BUILD_NUM"));
    put(
        t,
        "ci.pipeline.url",
        format!(
            "https://app.circleci.com/pipelines/workflows/{}",
            e.get("CIRCLE_WORKFLOW_ID")
        ),
    );
    put(t, "ci.job.name", e.get("CIRCLE_JOB"));
    put(t, "ci.job.id", e.get("CIRCLE_BUILD_NUM"));
    put(t, "ci.job.url", e.get("CIRCLE_BUILD_URL"));
    put(t, "pr.number", e.get("CIRCLE_PR_NUMBER"));
    put(
        t,
        "_dd.ci.env_vars",
        e.env_json(&["CIRCLE_BUILD_NUM", "CIRCLE_WORKFLOW_ID"]),
    );
}

fn github_actions(e: &Reader<'_>, t: &mut Tags) {
    let (branch, tag) = split_branch_or_tag(e.first(&["GITHUB_HEAD_REF", "GITHUB_REF"]));
    let mut server = e.get("GITHUB_SERVER_URL");
    if server.is_empty() {
        server = "https://github.com".into();
    }
    let repository = format!("{}/{}", server.trim_end_matches('/'), e.get("GITHUB_REPOSITORY"));
    let pipeline_id = e.get("GITHUB_RUN_ID");
    let sha = e.get("GITHUB_SHA");
    put(t, "ci.provider.name", "github".into());
    put(t, "git.repository_url", format!("{repository}.git"));
    put(t, "git.commit.sha", sha.clone());
    put(t, "git.branch", branch);
    put(t, "git.tag", tag);
    put(t, "ci.workspace_path", e.get("GITHUB_WORKSPACE"));
    put(t, "ci.pipeline.number", e.get("GITHUB_RUN_NUMBER"));
    put(t, "ci.pipeline.name", e.get("GITHUB_WORKFLOW"));
    if !pipeline_id.is_empty() {
        put(t, "ci.pipeline.id", pipeline_id.clone());
        let attempt = e.get("GITHUB_RUN_ATTEMPT");
        let url = if attempt.is_empty() {
            format!("{repository}/actions/runs/{pipeline_id}")
        } else {
            format!("{repository}/actions/runs/{pipeline_id}/attempts/{attempt}")
        };
        put(t, "ci.pipeline.url", url);
    }
    let job_name = e.get("GITHUB_JOB");
    let job_id = e.get("JOB_CHECK_RUN_ID").trim().to_string();
    put(t, "ci.job.name", job_name.clone());
    if !job_id.is_empty() && job_id.bytes().all(|b| b.is_ascii_digit()) && !pipeline_id.is_empty() {
        put(
            t,
            "ci.job.url",
            format!("{repository}/actions/runs/{pipeline_id}/job/{job_id}"),
        );
        put(t, "ci.job.id", job_id);
    } else {
        put(t, "ci.job.id", job_name);
        put(t, "ci.job.url", format!("{repository}/commit/{sha}/checks"));
    }
    put(
        t,
        "_dd.ci.env_vars",
        e.env_json(&[
            "GITHUB_SERVER_URL",
            "GITHUB_REPOSITORY",
            "GITHUB_RUN_ID",
            "GITHUB_RUN_ATTEMPT",
        ]),
    );
    let event_path = e.get("GITHUB_EVENT_PATH");
    if !event_path.is_empty() {
        if let Some(event) = std::fs::read_to_string(&event_path)
            .ok()
            .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        {
            let field = |path: &str| event.pointer(path).and_then(Value::as_str).unwrap_or("").to_string();
            put(t, "git.commit.head.sha", field("/pull_request/head/sha"));
            put(
                t,
                "git.pull_request.base_branch_head_sha",
                field("/pull_request/base/sha"),
            );
            put(t, "git.pull_request.base_branch", field("/pull_request/base/ref"));
            if let Some(number) = event.get("number").and_then(Value::as_f64) {
                put(t, "pr.number", format!("{}", number as i64));
            }
        }
    }
    if t.get("git.pull_request.base_branch").is_none_or(|v| v.is_empty()) {
        put(t, "git.pull_request.base_branch", e.get("GITHUB_BASE_REF"));
    }
}

fn gitlab(e: &Reader<'_>, t: &mut Tags) {
    put(t, "ci.provider.name", "gitlab".into());
    put(t, "git.repository_url", e.get("CI_REPOSITORY_URL"));
    put(t, "git.commit.sha", e.get("CI_COMMIT_SHA"));
    put(t, "git.branch", e.first(&["CI_COMMIT_BRANCH", "CI_COMMIT_REF_NAME"]));
    put(t, "git.tag", e.get("CI_COMMIT_TAG"));
    put(t, "ci.workspace_path", e.get("CI_PROJECT_DIR"));
    put(t, "ci.pipeline.id", e.get("CI_PIPELINE_ID"));
    put(t, "ci.pipeline.name", e.get("CI_PROJECT_PATH"));
    put(t, "ci.pipeline.number", e.get("CI_PIPELINE_IID"));
    put(t, "ci.pipeline.url", e.get("CI_PIPELINE_URL"));
    put(t, "ci.job.url", e.get("CI_JOB_URL"));
    put(t, "ci.job.id", e.get("CI_JOB_ID"));
    put(t, "ci.job.name", e.get("CI_JOB_NAME"));
    put(t, "ci.stage.name", e.get("CI_JOB_STAGE"));
    put(t, "git.commit.message", e.get("CI_COMMIT_MESSAGE"));
    put(t, "ci.node.name", e.get("CI_RUNNER_ID"));
    put(t, "ci.node.labels", e.get("CI_RUNNER_TAGS"));
    let author = e.get("CI_COMMIT_AUTHOR");
    let mut parts = author.split(['<', '>']).filter(|s| !s.is_empty());
    put(
        t,
        "git.commit.author.name",
        parts.next().unwrap_or("").trim().to_string(),
    );
    put(
        t,
        "git.commit.author.email",
        parts.next().unwrap_or("").trim().to_string(),
    );
    put(t, "git.commit.author.date", e.get("CI_COMMIT_TIMESTAMP"));
    put(
        t,
        "_dd.ci.env_vars",
        e.env_json(&["CI_PROJECT_URL", "CI_PIPELINE_ID", "CI_JOB_ID"]),
    );
    put(t, "git.commit.head.sha", e.get("CI_MERGE_REQUEST_SOURCE_BRANCH_SHA"));
    put(
        t,
        "git.pull_request.base_branch_head_sha",
        e.get("CI_MERGE_REQUEST_TARGET_BRANCH_SHA"),
    );
    put(
        t,
        "git.pull_request.base_branch_sha",
        e.get("CI_MERGE_REQUEST_DIFF_BASE_SHA"),
    );
    put(
        t,
        "git.pull_request.base_branch",
        e.get("CI_MERGE_REQUEST_TARGET_BRANCH_NAME"),
    );
    put(t, "pr.number", e.get("CI_MERGE_REQUEST_IID"));
}

fn jenkins(e: &Reader<'_>, t: &mut Tags) {
    put(t, "ci.provider.name", "jenkins".into());
    put(t, "git.repository_url", e.first(&["GIT_URL", "GIT_URL_1"]));
    put(t, "git.commit.sha", e.get("GIT_COMMIT"));
    let branch_or_tag = e.get("GIT_BRANCH");
    let job_name = e.0.get("JOB_NAME");
    let mut name = job_name.clone().unwrap_or_default();
    if branch_or_tag.contains("tags/") {
        put(t, "git.tag", branch_or_tag);
    } else {
        name = name.replace(&format!("/{}", normalize_ref(&branch_or_tag)), "");
        put(t, "git.branch", branch_or_tag);
    }
    if job_name.is_some() {
        name = name
            .split('/')
            .enumerate()
            .filter(|(i, segment)| *i == 0 || !segment.contains('='))
            .map(|(_, segment)| segment)
            .collect::<Vec<_>>()
            .join("/");
    }
    put(t, "ci.workspace_path", e.get("WORKSPACE"));
    put(t, "ci.pipeline.id", e.get("BUILD_TAG"));
    put(t, "ci.pipeline.number", e.get("BUILD_NUMBER"));
    put(t, "ci.pipeline.name", name);
    put(t, "ci.pipeline.url", e.get("BUILD_URL"));
    put(t, "ci.node.name", e.get("NODE_NAME"));
    put(t, "pr.number", e.get("CHANGE_ID"));
    put(t, "git.pull_request.base_branch", e.get("CHANGE_TARGET"));
    put(
        t,
        "_dd.ci.env_vars",
        e.env_json(&["DD_CUSTOM_TRACE_ID", "DD_CUSTOM_PARENT_ID"]),
    );
    let labels = e.get("NODE_LABELS");
    if !labels.is_empty() {
        let items: Vec<&str> = labels.split(' ').collect();
        put(t, "ci.node.labels", serde_json::to_string(&items).unwrap_or_default());
    }
}

fn teamcity(e: &Reader<'_>, t: &mut Tags) {
    put(t, "ci.provider.name", "teamcity".into());
    put(t, "ci.job.url", e.get("BUILD_URL"));
    put(t, "ci.job.name", e.get("TEAMCITY_BUILDCONF_NAME"));
    put(t, "pr.number", e.get("TEAMCITY_PULLREQUEST_NUMBER"));
    put(
        t,
        "git.pull_request.base_branch",
        e.get("TEAMCITY_PULLREQUEST_TARGET_BRANCH"),
    );
}

fn codefresh(e: &Reader<'_>, t: &mut Tags) {
    put(t, "ci.provider.name", "codefresh".into());
    put(t, "ci.pipeline.id", e.get("CF_BUILD_ID"));
    put(t, "ci.pipeline.name", e.get("CF_PIPELINE_NAME"));
    put(t, "ci.pipeline.url", e.get("CF_BUILD_URL"));
    put(t, "ci.job.name", e.get("CF_STEP_NAME"));
    put(t, "_dd.ci.env_vars", e.env_json(&["CF_BUILD_ID"]));
    let branch = e.get("CF_BRANCH");
    let key = if branch.contains("tags/") {
        "git.tag"
    } else {
        "git.branch"
    };
    put(t, key, normalize_ref(&branch));
    put(t, "git.pull_request.base_branch", e.get("CF_PULL_REQUEST_TARGET"));
    put(t, "pr.number", e.get("CF_PULL_REQUEST_NUMBER"));
}

fn travis(e: &Reader<'_>, t: &mut Tags) {
    let mut slug = e.get("TRAVIS_PULL_REQUEST_SLUG");
    if slug.trim().is_empty() {
        slug = e.get("TRAVIS_REPO_SLUG");
    }
    put(t, "ci.provider.name", "travisci".into());
    put(t, "git.repository_url", format!("https://github.com/{slug}.git"));
    put(t, "git.commit.sha", e.get("TRAVIS_COMMIT"));
    put(t, "git.tag", e.get("TRAVIS_TAG"));
    put(
        t,
        "git.branch",
        e.first(&["TRAVIS_PULL_REQUEST_BRANCH", "TRAVIS_BRANCH"]),
    );
    put(t, "ci.workspace_path", e.get("TRAVIS_BUILD_DIR"));
    put(t, "ci.pipeline.id", e.get("TRAVIS_BUILD_ID"));
    put(t, "ci.pipeline.number", e.get("TRAVIS_BUILD_NUMBER"));
    put(t, "ci.pipeline.name", slug);
    put(t, "ci.pipeline.url", e.get("TRAVIS_BUILD_WEB_URL"));
    put(t, "ci.job.url", e.get("TRAVIS_JOB_WEB_URL"));
    put(t, "git.commit.message", e.get("TRAVIS_COMMIT_MESSAGE"));
    put(t, "git.pull_request.base_branch", e.get("TRAVIS_BRANCH"));
    put(t, "git.commit.head.sha", e.get("TRAVIS_PULL_REQUEST_SHA"));
    put(t, "pr.number", e.get("TRAVIS_PULL_REQUEST"));
}

fn aws_codepipeline(e: &Reader<'_>, t: &mut Tags) {
    if !e.get("CODEBUILD_INITIATOR").starts_with("codepipeline") {
        return;
    }
    put(t, "ci.provider.name", "awscodepipeline".into());
    put(t, "ci.pipeline.id", e.get("DD_PIPELINE_EXECUTION_ID"));
    put(t, "ci.job.id", e.get("DD_ACTION_EXECUTION_ID"));
    put(
        t,
        "_dd.ci.env_vars",
        e.env_json(&[
            "CODEBUILD_BUILD_ARN",
            "DD_ACTION_EXECUTION_ID",
            "DD_PIPELINE_EXECUTION_ID",
        ]),
    );
}

fn drone(e: &Reader<'_>, t: &mut Tags) {
    put(t, "ci.provider.name", "drone".into());
    put(t, "git.branch", e.get("DRONE_BRANCH"));
    put(t, "git.commit.sha", e.get("DRONE_COMMIT_SHA"));
    put(t, "git.repository_url", e.get("DRONE_GIT_HTTP_URL"));
    put(t, "git.tag", e.get("DRONE_TAG"));
    put(t, "ci.pipeline.number", e.get("DRONE_BUILD_NUMBER"));
    put(t, "ci.pipeline.url", e.get("DRONE_BUILD_LINK"));
    put(t, "git.commit.message", e.get("DRONE_COMMIT_MESSAGE"));
    put(t, "git.commit.author.name", e.get("DRONE_COMMIT_AUTHOR_NAME"));
    put(t, "git.commit.author.email", e.get("DRONE_COMMIT_AUTHOR_EMAIL"));
    put(t, "ci.workspace_path", e.get("DRONE_WORKSPACE"));
    put(t, "ci.job.name", e.get("DRONE_STEP_NAME"));
    put(t, "ci.stage.name", e.get("DRONE_STAGE_NAME"));
    put(t, "pr.number", e.get("DRONE_PULL_REQUEST"));
    put(t, "git.pull_request.base_branch", e.get("DRONE_TARGET_BRANCH"));
}

const COMMIT_TAGS: [&str; 8] = [
    "git.commit.sha",
    "git.commit.author.name",
    "git.commit.author.email",
    "git.commit.author.date",
    "git.commit.committer.name",
    "git.commit.committer.email",
    "git.commit.committer.date",
    "git.commit.message",
];

fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Reads git tags from the local repository. The remote URL and the commit
/// details are shared through `cache` by the agents that see the same checkout.
pub fn local_git_tags(dir: &Path, cache: Option<&ReadCache>) -> Tags {
    let mut tags = Tags::new();
    let head = git(dir, &["rev-parse", "--show-toplevel", "HEAD", "--abbrev-ref", "HEAD"]).unwrap_or_default();
    let head: Vec<&str> = head.lines().collect();
    let [root, sha, branch] = head[..] else {
        if let Some(root) = git(dir, &["rev-parse", "--show-toplevel"]) {
            tags.insert("ci.workspace_path".into(), root);
            tags.extend(remote_url(dir));
        }
        return tags;
    };
    tags.insert("ci.workspace_path".into(), root.to_string());
    if branch != "HEAD" {
        tags.insert("git.branch".into(), branch.to_string());
    }
    let details = || {
        let mut details = remote_url(dir);
        details.extend(commit(dir));
        details
    };
    let cached = cache
        .and_then(|cache| {
            let key = json!({ "root": root, "sha": sha, "branch": branch });
            cache
                .get_or_fetch("git", &key, || serde_json::to_value(details()).map(|value| (value, 0)))
                .ok()
        })
        .and_then(|(value, _)| serde_json::from_value(value).ok());
    let details = cached.unwrap_or_else(details);
    tags.extend(details);
    tags
}

fn remote_url(dir: &Path) -> Tags {
    git(dir, &["ls-remote", "--get-url"])
        .filter(|u| !u.is_empty() && u != "origin")
        .map(|url| Tags::from([("git.repository_url".to_string(), filter_sensitive_info(&url))]))
        .unwrap_or_default()
}

fn commit(dir: &Path) -> Tags {
    let Some(log) = git(
        dir,
        &[
            "log",
            "-1",
            "--format=%H%x00%an%x00%ae%x00%aI%x00%cn%x00%ce%x00%cI%x00%B",
        ],
    ) else {
        return Tags::new();
    };
    COMMIT_TAGS
        .iter()
        .zip(log.splitn(8, '\0'))
        .filter(|(_, value)| !value.trim().is_empty())
        .map(|(key, value)| (key.to_string(), value.trim().to_string()))
        .collect()
}

/// Provider tags, completed with local git data when it describes the same commit.
pub fn collect(env: &dyn Env, mut tags: Tags, dir: &Path, cache: Option<&ReadCache>) -> Tags {
    let complete = ["ci.workspace_path", "git.repository_url", "git.branch"]
        .iter()
        .chain(&COMMIT_TAGS)
        .all(|key| tags.contains_key(*key));
    let local = if complete {
        Tags::new()
    } else {
        local_git_tags(dir, cache)
    };
    let same_commit = match (tags.get("git.commit.sha"), local.get("git.commit.sha")) {
        (Some(ci), Some(git)) => ci == git,
        _ => true,
    };
    for (key, value) in local {
        let commit_specific = key.starts_with("git.commit.") || key == "git.branch";
        if !same_commit && commit_specific {
            continue;
        }
        tags.entry(key).or_insert(value);
    }
    apply_user_supplied(env, &mut tags);
    normalize(&mut tags);
    tags.retain(|_, v| !v.is_empty());
    tags
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filters_credentials_from_urls() {
        assert_eq!(
            filter_sensitive_info("https://user:pwd@github.com/x.git"),
            "https://github.com/x.git"
        );
        assert_eq!(filter_sensitive_info("ssh://user@host:22/repo"), "ssh://host:22/repo");
        assert_eq!(
            filter_sensitive_info("git@github.com:org/repo.git"),
            "git@github.com:org/repo.git"
        );
        assert_eq!(
            filter_sensitive_info(r#"{"a":"https://u@x.com/p","b":"http://v@y.com"}"#),
            r#"{"a":"https://x.com/p","b":"http://y.com"}"#
        );
    }

    fn repo(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("dd-test-git-{name}-{:x}", crate::ids::new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let run = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .args([
                    "-c",
                    "user.name=A",
                    "-c",
                    "user.email=a@x",
                    "-c",
                    "commit.gpgsign=false",
                ])
                .args(args)
                .current_dir(&dir)
                .output()
                .unwrap()
                .status;
            assert!(status.success(), "git {args:?}");
        };
        run(&["init", "-q", "-b", "main"]);
        run(&["remote", "add", "origin", "https://user:pwd@example.com/DataDog/a.git"]);
        dir
    }

    fn commit_all(dir: &Path) {
        let output = std::process::Command::new("git")
            .args([
                "-c",
                "user.name=A",
                "-c",
                "user.email=a@x",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(["commit", "-q", "--allow-empty", "-m", "first"])
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(output.status.success());
    }

    fn cache(dir: &Path) -> ReadCache {
        let config = crate::config::Config::from_lookup(|_| None);
        let scope = crate::read_cache::Scope::select(&Tags::new(), None, Some(1)).unwrap();
        ReadCache::new(dir.join("cache"), &scope, &config, "0.1.0")
    }

    #[test]
    fn reads_local_git_tags() {
        let dir = repo("tags");
        let empty = local_git_tags(&dir, None);
        assert_eq!(empty["git.repository_url"], "https://example.com/DataDog/a.git");
        assert!(empty.contains_key("ci.workspace_path"));
        assert!(!empty.contains_key("git.commit.sha"));

        commit_all(&dir);
        let tags = local_git_tags(&dir, None);
        assert_eq!(tags["git.repository_url"], "https://example.com/DataDog/a.git");
        assert_eq!(tags["git.branch"], "main");
        assert_eq!(tags["git.commit.sha"].len(), 40);
        assert_eq!(tags["git.commit.author.email"], "a@x");
        assert_eq!(tags["git.commit.message"], "first");
        assert!(local_git_tags(&std::env::temp_dir().join("dd-test-git-missing"), None).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn shares_local_git_tags_per_commit() {
        let dir = repo("shared");
        commit_all(&dir);
        let cache = cache(&dir);
        let first = local_git_tags(&dir, Some(&cache));
        assert_eq!(first, local_git_tags(&dir, None));

        let set_url = |url: &str| {
            std::process::Command::new("git")
                .args(["remote", "set-url", "origin", url])
                .current_dir(&dir)
                .status()
                .unwrap()
        };
        set_url("https://example.com/DataDog/b.git");
        assert_eq!(
            local_git_tags(&dir, Some(&cache)),
            first,
            "the same commit is read from the cache"
        );

        commit_all(&dir);
        let second = local_git_tags(&dir, Some(&cache));
        assert_ne!(second["git.commit.sha"], first["git.commit.sha"]);
        assert_eq!(second["git.repository_url"], "https://example.com/DataDog/b.git");
        let _ = std::fs::remove_dir_all(&dir);
    }

    struct MapEnv(Tags);

    impl Env for MapEnv {
        fn get(&self, key: &str) -> Option<String> {
            self.0.get(key).cloned()
        }

        fn vars(&self) -> Vec<(String, String)> {
            self.0.clone().into_iter().collect()
        }
    }

    #[test]
    fn skips_git_when_the_tags_are_supplied() {
        let dir = repo("supplied");
        commit_all(&dir);
        let mut supplied: Tags = ["ci.workspace_path", "git.repository_url", "git.branch"]
            .iter()
            .chain(&COMMIT_TAGS)
            .map(|key| (key.to_string(), format!("supplied {key}")))
            .collect();
        let env = MapEnv(Tags::new());
        let cache = cache(&dir);
        assert_eq!(collect(&env, supplied.clone(), &dir, Some(&cache)), supplied);
        assert!(!dir.join("cache").exists(), "git did not run, so nothing was cached");

        supplied.remove("git.commit.message");
        let tags = collect(&env, supplied, &dir, Some(&cache));
        assert!(dir.join("cache").exists(), "a missing tag makes git run");
        assert_eq!(tags["git.repository_url"], "supplied git.repository_url");
        assert!(
            !tags.contains_key("git.commit.message"),
            "local tags of another commit are not used"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn normalizes_refs() {
        assert_eq!(normalize_ref("refs/heads/main"), "main");
        assert_eq!(normalize_ref("origin/tags/v1"), "v1");
        assert_eq!(normalize_ref("feature/x"), "feature/x");
    }
}
