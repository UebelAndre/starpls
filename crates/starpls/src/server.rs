use std::collections::HashMap;
use std::fs;
use std::mem;
use std::panic;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use log::debug;
use log::error;
use log::info;
use log::warn;
use lsp_server::Connection;
use lsp_server::ReqQueue;
use lsp_server::RequestId;
use lsp_server::Response;
use lsp_types::request::Request as _;
use parking_lot::RwLock;
use rustc_hash::FxHashMap;
use rustc_hash::FxHashSet;
use starpls_bazel::build_language::decode_rules;
use starpls_bazel::client::take_missing_host_data;
use starpls_bazel::client::BazelCLI;
use starpls_bazel::client::BazelClient;
use starpls_bazel::client::HostData;
use starpls_bazel::client::StaticBazelClient;
use starpls_bazel::decode_builtins;
use starpls_bazel::APIContext;
use starpls_bazel::Builtins;
use starpls_common::Dialect;
use starpls_common::FileId;
use starpls_common::FileInfo;
use starpls_ide::Analysis;
use starpls_ide::AnalysisSnapshot;
use starpls_ide::Change;
use starpls_ide::InferenceOptions;

use crate::bazel::BazelContext;
use crate::config::ServerConfig;
use crate::debouncer::AnalysisDebouncer;
use crate::diagnostics::DiagnosticsManager;
use crate::document::DefaultFileLoader;
use crate::document::DocumentChangeKind;
use crate::document::DocumentManager;
use crate::document::PathInterner;
use crate::event_loop::FetchExternalReposProgress;
use crate::event_loop::RefreshAllWorkspaceTargetsProgress;
use crate::event_loop::Task;
use crate::extensions;
use crate::task_pool::TaskPool;
use crate::task_pool::TaskPoolHandle;

const BAZEL_INIT_ERR_MESSAGE: &str = "Failed to fetch Bazel configuration! Please check the language server logs for more details. Certain features may not work correctly until the underlying issue is fixed.";

pub(crate) struct Server {
    pub(crate) config: Arc<ServerConfig>,
    pub(crate) connection: Connection,
    pub(crate) req_queue: ReqQueue<(), OutgoingRequest>,
    pub(crate) task_pool_handle: TaskPoolHandle<Task>,
    pub(crate) document_manager: Arc<RwLock<DocumentManager>>,
    pub(crate) diagnostics_manager: DiagnosticsManager,
    pub(crate) analysis: Analysis,
    pub(crate) analysis_debouncer: AnalysisDebouncer,
    pub(crate) analysis_requested_for_files: Option<Vec<FileId>>,
    pub(crate) bazel_client: Arc<dyn BazelClient>,
    pub(crate) pending_repos: FxHashSet<String>,
    pub(crate) pending_files: FxHashSet<FileId>,
    pub(crate) force_analysis_for_files: FxHashSet<FileId>,
    pub(crate) fetched_repos: FxHashSet<String>,
    pub(crate) is_fetching_repos: bool,
    pub(crate) is_refreshing_all_workspace_targets: bool,
    pub(crate) bzlmod_enabled: bool,
    /// The client-supplied Bazel data, when that is where Bazel data comes from.
    pub(crate) host_client: Option<Arc<StaticBazelClient>>,
    /// Host data requested from the client and not answered yet.
    pub(crate) host_in_flight: FxHashMap<HostData, RequestId>,
    /// Host data the client said it cannot provide; not asked for again
    /// until the module file changes.
    /// Data the client could not provide, with when it said so. The server
    /// asks again after `HOST_RETRY_COOLDOWN`, since a busy Bazel at startup
    /// is not a permanent condition.
    pub(crate) host_unavailable: FxHashMap<HostData, Instant>,
    /// Bumped whenever the module graph may have changed; host replies from
    /// an earlier generation are ignored.
    pub(crate) host_generation: u64,
    /// Repositories the client declined to fetch; not asked for again until
    /// the module graph changes, and never reported as failures.
    pub(crate) declined_fetches: FxHashSet<String>,
    /// Requests waiting for host data before they run again.
    pub(crate) parked: Vec<ParkedRequest>,
    /// Requests that have waited once already; a second time they answer
    /// with what they have.
    pub(crate) parked_once: FxHashSet<RequestId>,
    /// A repository fetch delegated to the client, until every repository
    /// has been answered.
    pub(crate) host_fetch: Option<HostFetch>,
}

/// What an outgoing request was for, to act on its response. A host request
/// carries the generation it was sent in; a reply from an older generation
/// (sent before the module graph changed) is dropped.
pub(crate) enum OutgoingRequest {
    Plain,
    Host(HostData, u64),
}

pub(crate) struct ParkedRequest {
    req: lsp_server::Request,
    response: Response,
    waiting_on: Vec<HostData>,
    /// Whether any of the data waited on has arrived (as opposed to being
    /// reported unavailable).
    received: bool,
    since: Instant,
}

pub(crate) struct HostFetch {
    remaining: FxHashSet<String>,
    files: FxHashSet<FileId>,
    failed: Vec<String>,
}

/// How long a request waits for host data before answering without it.
/// In single-threaded mode this is checked when the next message arrives.
pub(crate) const HOST_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// How long data the client could not provide is treated as unavailable
/// before it is asked for again.
pub(crate) const HOST_RETRY_COOLDOWN: Duration = Duration::from_secs(60);

pub(crate) struct ServerSnapshot {
    pub(crate) config: Arc<ServerConfig>,
    pub(crate) analysis_snapshot: AnalysisSnapshot,
    pub(crate) document_manager: Arc<RwLock<DocumentManager>>,
}

impl Server {
    pub(crate) fn new(connection: Connection, config: ServerConfig) -> anyhow::Result<Self> {
        // Create the task pool for processing incoming requests.
        let (task_pool_sender, task_pool_receiver) = crossbeam_channel::unbounded();
        let inline = config.args.single_threaded || cfg!(target_family = "wasm");
        let task_pool =
            TaskPool::with_num_threads(task_pool_sender.clone(), if inline { 0 } else { 4 })?;
        let task_pool_handle = TaskPoolHandle::new(task_pool_receiver, task_pool);

        // Check if the user specified a path to the Bazel executable.
        let bazel_path = config
            .args
            .bazel_path
            .clone()
            .unwrap_or("bazel".to_string());

        debug!(
            "fetching Bazel configuration using Bazel executable at {:?}",
            bazel_path
        );

        // Determine Bazel configuration.
        let mut has_bazel_init_err = false;
        let mut host_client: Option<Arc<StaticBazelClient>> = None;
        let bazel_client: Arc<dyn BazelClient> = match config
            .initialization_options
            .as_ref()
            .and_then(crate::host::bazel_client_from_options)
        {
            Some(Ok(client)) => {
                info!("using the Bazel information provided by the client");
                let client = Arc::new(client);
                host_client = Some(client.clone());
                client
            }
            Some(Err(err)) => {
                error!("ignoring the client's Bazel information: {:#}", err);
                has_bazel_init_err = true;
                Arc::new(BazelCLI::new(&bazel_path))
            }
            None => Arc::new(BazelCLI::new(&bazel_path)),
        };
        let bazel_cx = match BazelContext::new(&*bazel_client) {
            Ok(cx) => cx,
            Err(err) => {
                has_bazel_init_err = true;
                error!("failed to initialize Bazel context: {}", err);
                Default::default()
            }
        };

        // Query for all targets in the current workspace, to use for label completion.
        let targets = if config.args.enable_label_completions {
            debug!("querying for all targets in the current workspace");
            match bazel_client.query_all_workspace_targets() {
                Ok(targets) => {
                    debug!("successfully queried for all targets");
                    targets
                }
                Err(err) => {
                    error!("failed to query all workspace targets: {}", err);
                    has_bazel_init_err = true;
                    Default::default()
                }
            }
        } else {
            Default::default()
        };

        let path_interner = Arc::new(PathInterner::default());
        let loader = DefaultFileLoader::new(
            bazel_client.clone(),
            path_interner.clone(),
            bazel_cx.info.workspace.clone(),
            bazel_cx.info.workspace_name,
            bazel_cx.info.output_base.join("external"),
            task_pool_sender.clone(),
            bazel_cx.bzlmod_enabled,
        );
        let mut analysis = Analysis::new(
            Arc::new(loader),
            InferenceOptions {
                infer_ctx_attributes: config.args.inference_options.infer_ctx_attributes,
                use_code_flow_analysis: config.args.inference_options.use_code_flow_analysis,
                ..Default::default()
            },
        );

        analysis.set_all_workspace_targets(targets);
        analysis.set_builtin_defs(load_bazel_builtins(), bazel_cx.rules);

        // Check for a prelude file. We skip verifying that `//tools/build_tools` is actually a package (i.e.
        // that it actually contains a `BUILD.bazel`) file for simplicity.
        if let Ok((prelude, contents)) = load_bazel_prelude(&bazel_cx.info.workspace) {
            info!("found prelude file at {:?}", prelude);
            let file_id = path_interner.intern_path(prelude);
            let mut change = Change::default();
            change.create_file(
                file_id,
                Dialect::Bazel,
                Some(FileInfo::Bazel {
                    api_context: APIContext::Bzl,
                    is_external: false,
                }),
                contents,
            );
            analysis.apply_change(change);
            analysis.set_bazel_prelude_file(file_id);
        }

        let analysis_debounce_interval = config.args.analysis_debounce_interval;
        let mut server = Server {
            config: Arc::new(config),
            connection,
            req_queue: Default::default(),
            task_pool_handle,
            document_manager: Arc::new(RwLock::new(DocumentManager::new(
                path_interner,
                bazel_cx.info.workspace,
            ))),
            diagnostics_manager: Default::default(),
            analysis,
            analysis_debouncer: AnalysisDebouncer::new(
                Duration::from_millis(analysis_debounce_interval),
                task_pool_sender,
                inline,
            ),
            analysis_requested_for_files: None,
            bazel_client,
            pending_repos: Default::default(),
            pending_files: Default::default(),
            force_analysis_for_files: Default::default(),
            fetched_repos: Default::default(),
            is_fetching_repos: false,
            is_refreshing_all_workspace_targets: false,
            bzlmod_enabled: bazel_cx.bzlmod_enabled,
            host_client,
            host_in_flight: Default::default(),
            host_unavailable: Default::default(),
            host_generation: 0,
            declined_fetches: Default::default(),
            parked: Vec::new(),
            parked_once: Default::default(),
            host_fetch: None,
        };

        if has_bazel_init_err {
            server.send_error_message(BAZEL_INIT_ERR_MESSAGE);
        }
        // Whatever the Bazel context could not get from the client up front
        // (typically the main repository mapping and the native rules) is
        // asked for now, in the background.
        for data in take_missing_host_data() {
            server.request_host_data(data);
        }

        Ok(server)
    }

    pub(crate) fn snapshot(&self) -> ServerSnapshot {
        ServerSnapshot {
            config: self.config.clone(),
            analysis_snapshot: self.analysis.snapshot(),
            document_manager: Arc::clone(&self.document_manager),
        }
    }

    pub(crate) fn process_changes(&mut self) -> (Vec<FileId>, bool) {
        let mut change = Change::default();
        let mut document_manager = self.document_manager.write();
        let (has_opened_or_closed_documents, changes) = document_manager.take_changes();
        let changed_file_ids = changes.iter().map(|(file_id, _)| *file_id).collect();

        if changes.is_empty() && self.force_analysis_for_files.is_empty() {
            return (changed_file_ids, has_opened_or_closed_documents);
        }

        let mut prelude_file = None;

        for (file_id, change_kind) in changes {
            let document = match document_manager.get(file_id) {
                Some(document) => document,
                None => continue,
            };
            match change_kind {
                DocumentChangeKind::Create => {
                    if matches!(
                        document.info,
                        Some(FileInfo::Bazel {
                            api_context: APIContext::Prelude,
                            ..
                        })
                    ) {
                        prelude_file = Some(file_id)
                    }

                    change.create_file(
                        file_id,
                        document.dialect,
                        document.info.clone(),
                        document.contents.clone(),
                    );
                }
                DocumentChangeKind::Update => {
                    change.update_file(file_id, document.contents.clone());
                }
            }
        }

        drop(document_manager);

        // Apply the change to our analyzer. This will cancel any affected active Salsa operations.
        self.analysis.apply_change(change);
        if let Some(prelude_file) = prelude_file {
            self.analysis.set_bazel_prelude_file(prelude_file);
        }

        (changed_file_ids, true)
    }

    pub(crate) fn send_request<R: lsp_types::request::Request>(&mut self, params: R::Params) {
        let req =
            self.req_queue
                .outgoing
                .register(R::METHOD.to_string(), params, OutgoingRequest::Plain);
        self.send(req.into());
    }

    pub(crate) fn complete_request(&mut self, resp: lsp_server::Response) {
        if let Some(OutgoingRequest::Host(data, generation)) =
            self.req_queue.outgoing.complete(resp.id.clone())
        {
            if generation != self.host_generation {
                debug!(
                    "ignoring the reply for {:?} from before the module graph changed",
                    data
                );
                return;
            }
            self.on_host_response(data, resp);
        }
    }

    /// Forgets everything learned from the client about the module graph:
    /// cached mappings, failed lookups and requests in flight, whose replies
    /// are stale now. Called when `MODULE.bazel` or a `WORKSPACE` changes.
    pub(crate) fn invalidate_host_data(&mut self) {
        self.bazel_client.clear_repo_mappings();
        self.fetched_repos.clear();
        self.host_unavailable.clear();
        self.host_in_flight.clear();
        self.host_generation += 1;
        self.declined_fetches.clear();
        // A fetch batch still waiting on the client will never hear back
        // (those replies are stale now); end it so fetching can start over.
        if let Some(fetch) = self.host_fetch.take() {
            let mut failed = fetch.failed;
            failed.extend(fetch.remaining);
            let files = fetch.files;
            self.task_pool_handle.spawn(move || {
                Task::FetchExternalRepos(FetchExternalReposProgress::End(files, failed))
            });
        }
    }

    /// Whether the client said recently that it cannot provide the data.
    fn host_unavailable_now(&self, data: &HostData) -> bool {
        self.host_unavailable
            .get(data)
            .is_some_and(|since| since.elapsed() < HOST_RETRY_COOLDOWN)
    }

    /// Asks the client for Bazel data the static client does not have, unless
    /// it is already on its way or the client said it cannot provide it.
    pub(crate) fn request_host_data(&mut self, data: HostData) {
        if !self
            .host_client
            .as_ref()
            .is_some_and(|host| host.client_requests())
        {
            return;
        }
        if self.host_unavailable_now(&data) || self.host_in_flight.contains_key(&data) {
            return;
        }
        if let HostData::FetchRepo(repo) = &data {
            if self.declined_fetches.contains(repo) {
                return;
            }
        }
        debug!("asking the client for {:?}", data);
        let req = match &data {
            HostData::RepoMapping(repo) => self.req_queue.outgoing.register(
                extensions::BazelRepoMapping::METHOD.to_string(),
                extensions::BazelRepoMappingParams { repo: repo.clone() },
                OutgoingRequest::Host(data.clone(), self.host_generation),
            ),
            HostData::BuildLanguage => self.req_queue.outgoing.register(
                extensions::BazelBuildLanguage::METHOD.to_string(),
                extensions::BazelBuildLanguageParams {},
                OutgoingRequest::Host(data.clone(), self.host_generation),
            ),
            HostData::FetchRepo(repo) => self.req_queue.outgoing.register(
                extensions::BazelFetchRepo::METHOD.to_string(),
                extensions::BazelFetchRepoParams { repo: repo.clone() },
                OutgoingRequest::Host(data.clone(), self.host_generation),
            ),
        };
        self.host_in_flight.insert(data, req.id.clone());
        self.send(req.into());
    }

    /// Answers a finished request, or holds it when Bazel data it may depend
    /// on is still to come from the client: data its handler found missing,
    /// or a repository mapping or the native rules requested earlier and not
    /// answered yet (a load resolution cached before they arrive is stale).
    /// A held request runs again when the data arrives. It waits at most once
    /// and at most `HOST_REQUEST_TIMEOUT`; after that it is answered with what
    /// the handler produced.
    pub(crate) fn park_request(
        &mut self,
        req: lsp_server::Request,
        response: Response,
        missing: Vec<HostData>,
    ) {
        // A miss recorded while the data was on its way is stale once the
        // data is installed; such a request runs again right away.
        let installed =
            |data: &HostData| self.host_client.as_ref().is_some_and(|host| host.has(data));
        // A miss for data that arrived in the meantime is a reason to run
        // the request again right away (once), not to wait.
        let installed_meanwhile = missing.iter().any(installed);
        let mut missing: Vec<HostData> = missing
            .into_iter()
            .filter(|data| !self.host_unavailable_now(data) && !installed(data))
            .collect();
        if missing.is_empty()
            && installed_meanwhile
            && self
                .host_client
                .as_ref()
                .is_some_and(|host| host.client_requests())
            && self.parked_once.insert(req.id.clone())
        {
            debug!("request {} runs again with data that just arrived", req.id);
            self.task_pool_handle.spawn(move || Task::Retry(req));
            return;
        }
        if missing.is_empty() && response_is_empty(&response) {
            // A handler that found nothing may have hit a resolution cached
            // before the main repository's mapping or the native rules
            // arrived, so an empty answer waits for those while they are in
            // flight. An answer with content stands; a request that needs
            // another repository's mapping says so itself through `missing`.
            missing.extend(
                self.host_in_flight
                    .keys()
                    .filter(|data| {
                        matches!(data, HostData::BuildLanguage)
                            || matches!(data, HostData::RepoMapping(repo) if repo.is_empty())
                    })
                    .cloned(),
            );
        }
        let can_wait = self
            .host_client
            .as_ref()
            .is_some_and(|host| host.client_requests())
            && !missing.is_empty()
            && self.parked_once.insert(req.id.clone());
        if !can_wait {
            self.respond(response);
            return;
        }
        debug!("request {} waits for {:?}", req.id, missing);
        for data in &missing {
            self.request_host_data(data.clone());
        }
        self.parked.push(ParkedRequest {
            req,
            response,
            waiting_on: missing,
            received: false,
            since: Instant::now(),
        });
    }

    /// When the oldest parked request is due to be answered without its
    /// data, if any is parked.
    pub(crate) fn next_parked_deadline(&self) -> Option<Instant> {
        self.parked
            .iter()
            .map(|parked| parked.since + HOST_REQUEST_TIMEOUT)
            .min()
    }

    /// Answers parked requests whose data has not arrived in time.
    pub(crate) fn expire_parked_requests(&mut self) {
        if self.parked.is_empty() {
            return;
        }
        let now = Instant::now();
        let (expired, waiting): (Vec<_>, Vec<_>) = mem::take(&mut self.parked)
            .into_iter()
            .partition(|parked| now.duration_since(parked.since) >= HOST_REQUEST_TIMEOUT);
        self.parked = waiting;
        for parked in expired {
            warn!(
                "answering request {} without {:?}: the client did not provide it within {:?}",
                parked.req.id, parked.waiting_on, HOST_REQUEST_TIMEOUT
            );
            self.respond(parked.response);
        }
    }

    /// Stores host data the client answered with, refreshes what depended on
    /// it, and re-runs the requests that were waiting.
    fn on_host_response(&mut self, data: HostData, resp: Response) {
        self.host_in_flight.remove(&data);
        let host = match &self.host_client {
            Some(host) => host.clone(),
            None => return,
        };
        let result = resp.result.unwrap_or(serde_json::Value::Null);
        let mut available = true;
        match &data {
            HostData::RepoMapping(repo) => {
                match serde_json::from_value::<Option<HashMap<String, String>>>(result) {
                    Ok(Some(mapping)) => {
                        info!(
                            "received the repository mapping of \"{}\" ({} entries)",
                            repo,
                            mapping.len()
                        );
                        host.set_repo_mapping(repo, mapping);
                    }
                    _ => available = false,
                }
            }
            HostData::BuildLanguage => match serde_json::from_value::<Option<String>>(result) {
                Ok(Some(path)) => match fs::read(&path) {
                    Ok(bytes) => {
                        host.set_build_language(bytes);
                        match load_bazel_build_language(&*host) {
                            Ok(rules) => {
                                info!("received the native rules from {}", path);
                                self.analysis.set_builtin_defs(load_bazel_builtins(), rules);
                            }
                            Err(err) => {
                                error!("failed to decode the native rules from {}: {}", path, err);
                                // Bytes that do not decode are not installed rules.
                                host.clear_build_language();
                                available = false;
                            }
                        }
                    }
                    Err(err) => {
                        error!("failed to read the native rules from {}: {}", path, err);
                        available = false;
                    }
                },
                _ => available = false,
            },
            HostData::FetchRepo(repo) => {
                // `true`: fetched; `false`: tried and failed; `null`: the
                // client declined, which is the user's decision, not an error.
                let outcome = serde_json::from_value::<Option<bool>>(result).unwrap_or(Some(false));
                let ok = outcome == Some(true);
                if outcome.is_none() {
                    info!("the client declined to fetch {}", repo);
                    self.declined_fetches.insert(repo.clone());
                } else if !ok {
                    available = false;
                }
                if let Some(fetch) = &mut self.host_fetch {
                    fetch.remaining.remove(repo);
                    if outcome == Some(false) {
                        fetch.failed.push(repo.clone());
                    }
                    if fetch.remaining.is_empty() {
                        let fetch = self.host_fetch.take().unwrap();
                        self.task_pool_handle.spawn(move || {
                            Task::FetchExternalRepos(FetchExternalReposProgress::End(
                                fetch.files,
                                fetch.failed,
                            ))
                        });
                    }
                }
            }
        }
        if !available {
            warn!(
                "the client could not provide {:?}; asking again after {:?} at the earliest",
                data, HOST_RETRY_COOLDOWN
            );
            self.host_unavailable.insert(data.clone(), Instant::now());
        }
        // Load resolutions cached during inference may now be wrong either
        // way; an empty change resets that cache, and the open documents are
        // re-analysed so their diagnostics follow.
        self.analysis.apply_change(Change::default());
        let open = self.document_manager.read().editor_file_ids();
        self.force_analysis_for_files.extend(open);
        // A request may wait for more than one piece of data (the main
        // mapping and the native rules, typically); it runs again once the
        // last of them is in, and only if something actually arrived.
        let mut still_waiting = Vec::new();
        for mut parked in mem::take(&mut self.parked) {
            if !parked.waiting_on.contains(&data) {
                still_waiting.push(parked);
                continue;
            }
            parked.waiting_on.retain(|waiting| waiting != &data);
            parked.received |= available;
            if !parked.waiting_on.is_empty() {
                still_waiting.push(parked);
            } else if parked.received && parked.since.elapsed() < HOST_REQUEST_TIMEOUT {
                let req = parked.req;
                self.task_pool_handle.spawn(move || Task::Retry(req));
            } else {
                // Nothing arrived, or the wait has already expired: the
                // answer computed without the data stands.
                self.respond(parked.response);
            }
        }
        self.parked = still_waiting;
    }

    pub(crate) fn send_notification<N: lsp_types::notification::Notification>(
        &self,
        params: N::Params,
    ) {
        let not = lsp_server::Notification::new(N::METHOD.to_string(), params);
        self.send(not.into());
    }

    pub(crate) fn send(&self, message: lsp_server::Message) {
        self.connection.sender.send(message).unwrap();
    }

    pub(crate) fn send_error_message(&self, message: &str) {
        self.send_notification::<lsp_types::notification::ShowMessage>(
            lsp_types::ShowMessageParams {
                message: message.to_string(),
                typ: lsp_types::MessageType::ERROR,
            },
        )
    }

    pub(crate) fn fetch_bazel_external_repos(&mut self) {
        if self
            .host_client
            .as_ref()
            .is_some_and(|host| host.client_requests())
        {
            self.fetch_external_repos_through_client();
            return;
        }
        let repos = mem::take(&mut self.pending_repos);
        let files = mem::take(&mut self.pending_files);
        let bazel_client = self.bazel_client.clone();
        let bzlmod_enabled = self.bzlmod_enabled;

        self.is_fetching_repos = true;
        self.fetched_repos.extend(repos.clone());
        self.task_pool_handle.spawn_with_sender(move |sender| {
            sender
                .send(Task::FetchExternalRepos(FetchExternalReposProgress::Begin(
                    repos.clone(),
                )))
                .unwrap();

            let mut failed_repos = vec![];

            for repo in &repos {
                debug!("fetching external repository \"@@{}\"", repo);
                if let Err(err) = if bzlmod_enabled {
                    bazel_client.fetch_repo(repo)
                } else {
                    bazel_client.null_query_external_repo_targets(repo)
                } {
                    failed_repos.push(repo.clone());
                    error!(
                        "failed to fetch external repository \"@@{}\": {}",
                        repo, err
                    );
                }
            }

            sender
                .send(Task::FetchExternalRepos(FetchExternalReposProgress::End(
                    files,
                    failed_repos,
                )))
                .unwrap();
        });
    }

    /// The fetch flow when the client runs Bazel: one `starpls/bazel/fetchRepo`
    /// request per repository, the usual progress and refresh around them.
    fn fetch_external_repos_through_client(&mut self) {
        let repos = mem::take(&mut self.pending_repos);
        let files = mem::take(&mut self.pending_files);
        self.is_fetching_repos = true;
        self.fetched_repos.extend(repos.clone());
        let begin = repos.clone();
        self.task_pool_handle
            .spawn(move || Task::FetchExternalRepos(FetchExternalReposProgress::Begin(begin)));
        let mut fetch = HostFetch {
            remaining: FxHashSet::default(),
            files,
            failed: Vec::new(),
        };
        for repo in repos {
            if self.declined_fetches.contains(&repo) {
                // The user said no to this one; nothing to report.
                continue;
            }
            self.request_host_data(HostData::FetchRepo(repo.clone()));
            if self
                .host_in_flight
                .contains_key(&HostData::FetchRepo(repo.clone()))
            {
                fetch.remaining.insert(repo);
            } else {
                // Not asked: the client already said it cannot fetch it.
                fetch.failed.push(repo);
            }
        }
        if fetch.remaining.is_empty() {
            self.task_pool_handle.spawn(move || {
                Task::FetchExternalRepos(FetchExternalReposProgress::End(fetch.files, fetch.failed))
            });
        } else {
            self.host_fetch = Some(fetch);
        }
    }

    pub(crate) fn refresh_all_workspace_targets(&mut self) {
        if self.is_refreshing_all_workspace_targets || !self.config.args.enable_label_completions {
            return;
        }

        let bazel_client = self.bazel_client.clone();

        self.is_refreshing_all_workspace_targets = true;
        self.task_pool_handle.spawn_with_sender(move |sender| {
            sender
                .send(Task::RefreshAllWorkspaceTargets(
                    RefreshAllWorkspaceTargetsProgress::Begin,
                ))
                .unwrap();

            let targets = match bazel_client.query_all_workspace_targets() {
                Ok(targets) => Some(targets),
                Err(err) => {
                    error!("failed to query all workspace targets: {}", err);
                    None
                }
            };

            sender
                .send(Task::RefreshAllWorkspaceTargets(
                    RefreshAllWorkspaceTargetsProgress::End(targets),
                ))
                .unwrap();
        });
    }
}

impl panic::RefUnwindSafe for ServerSnapshot {}

pub(crate) fn load_bazel_builtins() -> Builtins {
    let data = include_bytes!("builtin/builtin.pb");

    // We want to crash if the bundled protobuf file is ever invalid.
    decode_builtins(&data[..]).expect("bug: invalid builtin.pb")
}

pub(crate) fn load_bazel_build_language(client: &dyn BazelClient) -> anyhow::Result<Builtins> {
    let build_language_output = client.build_language()?;
    decode_rules(&build_language_output)
}

fn load_bazel_prelude(workspace: impl AsRef<Path>) -> anyhow::Result<(PathBuf, String)> {
    let prelude = workspace.as_ref().join("tools/build_rules/prelude_bazel");
    let contents = fs::read_to_string(&prelude)?;
    Ok((prelude, contents))
}

/// Whether a response carries no result: null, an empty list, or an empty
/// completion list.
fn response_is_empty(response: &Response) -> bool {
    if response.error.is_some() {
        return false;
    }
    match &response.result {
        None | Some(serde_json::Value::Null) => true,
        Some(serde_json::Value::Array(items)) => items.is_empty(),
        Some(serde_json::Value::Object(object)) => object
            .get("items")
            .and_then(|items| items.as_array())
            .is_some_and(|items| items.is_empty()),
        _ => false,
    }
}
