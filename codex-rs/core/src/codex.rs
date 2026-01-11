use std::collections::HashMap;
use std::fmt::Debug;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use crate::AuthManager;
use crate::SandboxState;
use crate::client_common::REVIEW_PROMPT;
use crate::compact;
use crate::compact::run_inline_auto_compact_task;
use crate::compact::should_use_remote_compact_task;
use crate::compact_remote::run_inline_remote_auto_compact_task;
use crate::exec_policy::ExecPolicyManager;
use crate::features::Feature;
use crate::features::Features;
use crate::models_manager::manager::ModelsManager;
use crate::models_manager::model_family::ModelFamily;
use crate::parse_command::parse_command;
use crate::parse_turn_item;
use crate::stream_events_utils::HandleOutputCtx;
use crate::stream_events_utils::handle_non_tool_response_item;
use crate::stream_events_utils::handle_output_item_done;
use crate::terminal;
use crate::truncate::TruncationPolicy;
use crate::user_notification::UserNotifier;
use crate::util::error_or_panic;
use async_channel::Receiver;
use async_channel::Sender;
use codex_protocol::ConversationId;
use codex_protocol::approvals::ExecPolicyAmendment;
use codex_protocol::items::TurnItem;
use codex_protocol::protocol::FileChange;
use codex_protocol::protocol::HasLegacyEvent;
use codex_protocol::protocol::ItemCompletedEvent;
use codex_protocol::protocol::ItemStartedEvent;
use codex_protocol::protocol::RawResponseItemEvent;
use codex_protocol::protocol::ReviewRequest;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::TaskStartedEvent;
use codex_protocol::protocol::TurnAbortReason;
use codex_protocol::protocol::TurnContextItem;
use codex_rmcp_client::ElicitationResponse;
use futures::future::BoxFuture;
use futures::prelude::*;
use futures::stream::FuturesOrdered;
use mcp_types::CallToolResult;
use mcp_types::ListResourceTemplatesRequestParams;
use mcp_types::ListResourceTemplatesResult;
use mcp_types::ListResourcesRequestParams;
use mcp_types::ListResourcesResult;
use mcp_types::ReadResourceRequestParams;
use mcp_types::ReadResourceResult;
use mcp_types::RequestId;
use serde_json;
use serde_json::Value;
use tokio::sync::Mutex;
use tokio::sync::RwLock;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;
use tracing::Instrument;
use tracing::debug;
use tracing::error;
use tracing::field;
use tracing::info;
use tracing::instrument;
use tracing::trace_span;
use tracing::warn;

use crate::ModelProviderInfo;
use crate::WireApi;
use crate::client::ModelClient;
use crate::client_common::Prompt;
use crate::client_common::ResponseEvent;
use crate::compact::collect_user_messages;
use crate::config::Config;
use crate::config::Constrained;
use crate::config::ConstraintResult;
use crate::config::GhostSnapshotConfig;
use crate::config::types::ShellEnvironmentPolicy;
use crate::context_manager::ContextManager;
use crate::environment_context::EnvironmentContext;
use crate::error::CodexErr;
use crate::error::Result as CodexResult;
#[cfg(test)]
use crate::exec::StreamOutput;
use crate::exec_policy::ExecPolicyUpdateError;
use crate::mcp::auth::compute_auth_statuses;
use crate::mcp_connection_manager::McpConnectionManager;
use crate::model_provider_info::CHAT_WIRE_API_DEPRECATION_SUMMARY;
use crate::project_doc::get_user_instructions;
use crate::protocol::AgentMessageContentDeltaEvent;
use crate::protocol::AgentReasoningSectionBreakEvent;
use crate::protocol::ApplyPatchApprovalRequestEvent;
use crate::protocol::AskForApproval;
use crate::protocol::BackgroundEventEvent;
use crate::protocol::DeprecationNoticeEvent;
use crate::protocol::ErrorEvent;
use crate::protocol::Event;
use crate::protocol::EventMsg;
use crate::protocol::ExecApprovalRequestEvent;
use crate::protocol::Op;
use crate::protocol::RateLimitSnapshot;
use crate::protocol::ReasoningContentDeltaEvent;
use crate::protocol::ReasoningRawContentDeltaEvent;
use crate::protocol::ReviewDecision;
use crate::protocol::SandboxPolicy;
use crate::protocol::SessionConfiguredEvent;
use crate::protocol::SkillErrorInfo;
use crate::protocol::SkillMetadata as ProtocolSkillMetadata;
use crate::protocol::StreamErrorEvent;
use crate::protocol::Submission;
use crate::protocol::TokenCountEvent;
use crate::protocol::TokenUsage;
use crate::protocol::TokenUsageInfo;
use crate::protocol::TurnDiffEvent;
use crate::protocol::WarningEvent;
use crate::rollout::RolloutRecorder;
use crate::rollout::RolloutRecorderParams;
use crate::rollout::map_session_init_error;
use crate::shell;
use crate::shell_snapshot::ShellSnapshot;
use crate::skills::SkillError;
use crate::skills::SkillInjections;
use crate::skills::SkillMetadata;
use crate::skills::SkillsManager;
use crate::skills::build_skill_injections;
use crate::state::ActiveTurn;
use crate::state::SessionServices;
use crate::state::SessionState;
use crate::tasks::GhostSnapshotTask;
use crate::tasks::ReviewTask;
use crate::tasks::SessionTask;
use crate::tasks::SessionTaskContext;
use crate::tools::ToolRouter;
use crate::tools::context::SharedTurnDiffTracker;
use crate::tools::parallel::ToolCallRuntime;
use crate::tools::sandboxing::ApprovalStore;
use crate::tools::spec::ToolsConfig;
use crate::tools::spec::ToolsConfigParams;
use crate::turn_diff_tracker::TurnDiffTracker;
use crate::unified_exec::UnifiedExecSessionManager;
use crate::user_instructions::DeveloperInstructions;
use crate::user_instructions::UserInstructions;
use crate::user_notification::UserNotification;
use crate::util::backoff;
use codex_async_utils::OrCancelExt;
use codex_otel::otel_manager::OtelManager;
use codex_protocol::config_types::ReasoningSummary as ReasoningSummaryConfig;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseInputItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::openai_models::ReasoningEffort as ReasoningEffortConfig;
use codex_protocol::protocol::CodexErrorInfo;
use codex_protocol::protocol::InitialHistory;
use codex_protocol::user_input::UserInput;
use codex_utils_readiness::Readiness;
use codex_utils_readiness::ReadinessFlag;

/// The high-level interface to the Codex system.
/// It operates as a queue pair where you send submissions and receive events.
pub struct Codex {
    pub(crate) next_id: AtomicU64,
    pub(crate) tx_sub: Sender<Submission>,
    pub(crate) rx_event: Receiver<Event>,
}

/// Wrapper returned by [`Codex::spawn`] containing the spawned [`Codex`],
/// the submission id for the initial `ConfigureSession` request and the
/// unique session id.
pub struct CodexSpawnOk {
    pub codex: Codex,
    pub conversation_id: ConversationId,
}

pub(crate) const INITIAL_SUBMIT_ID: &str = "";
pub(crate) const SUBMISSION_CHANNEL_CAPACITY: usize = 64;
static CHAT_WIRE_API_DEPRECATION_EMITTED: AtomicBool = AtomicBool::new(false);

fn maybe_push_chat_wire_api_deprecation(
    config: &Config,
    post_session_configured_events: &mut Vec<Event>,
) {
    if config.model_provider.wire_api != WireApi::Chat {
        return;
    }

    if CHAT_WIRE_API_DEPRECATION_EMITTED
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return;
    }

    post_session_configured_events.push(Event {
        id: INITIAL_SUBMIT_ID.to_owned(),
        msg: EventMsg::DeprecationNotice(DeprecationNoticeEvent {
            summary: CHAT_WIRE_API_DEPRECATION_SUMMARY.to_string(),
            details: None,
        }),
    });
}

impl Codex {
    /// Spawn a new [`Codex`] and initialize the session.
    pub async fn spawn(
        config: Config,
        auth_manager: Arc<AuthManager>,
        models_manager: Arc<ModelsManager>,
        skills_manager: Arc<SkillsManager>,
        conversation_history: InitialHistory,
        session_source: SessionSource,
    ) -> CodexResult<CodexSpawnOk> {
        let (tx_sub, rx_sub) = async_channel::bounded(SUBMISSION_CHANNEL_CAPACITY);
        let (tx_event, rx_event) = async_channel::unbounded();

        let loaded_skills = config
            .features
            .enabled(Feature::Skills)
            .then(|| skills_manager.skills_for_cwd(&config.cwd));

        if let Some(outcome) = &loaded_skills {
            for err in &outcome.errors {
                error!(
                    "failed to load skill {}: {}",
                    err.path.display(),
                    err.message
                );
            }
        }

        let user_instructions = get_user_instructions(
            &config,
            loaded_skills
                .as_ref()
                .map(|outcome| outcome.skills.as_slice()),
        )
        .await;

        let exec_policy = ExecPolicyManager::load(&config.features, &config.config_layer_stack)
            .await
            .map_err(|err| CodexErr::Fatal(format!("failed to load execpolicy: {err}")))?;

        let config = Arc::new(config);
        if config.features.enabled(Feature::RemoteModels)
            && let Err(err) = models_manager
                .refresh_available_models_with_cache(&config)
                .await
        {
            error!("failed to refresh available models: {err:?}");
        }
        let model = models_manager.get_model(&config.model, &config).await;
        let session_configuration = SessionConfiguration {
            provider: config.model_provider.clone(),
            model: model.clone(),
            model_reasoning_effort: config.model_reasoning_effort,
            model_reasoning_summary: config.model_reasoning_summary,
            developer_instructions: config.developer_instructions.clone(),
            user_instructions,
            base_instructions: config.base_instructions.clone(),
            compact_prompt: config.compact_prompt.clone(),
            approval_policy: config.approval_policy.clone(),
            sandbox_policy: config.sandbox_policy.clone(),
            cwd: config.cwd.clone(),
            original_config_do_not_use: Arc::clone(&config),
            session_source,
        };

        // Generate a unique ID for the lifetime of this Codex session.
        let session_source_clone = session_configuration.session_source.clone();

        let session = Session::new(
            session_configuration,
            config.clone(),
            auth_manager.clone(),
            models_manager.clone(),
            exec_policy,
            tx_event.clone(),
            conversation_history,
            session_source_clone,
            skills_manager,
        )
        .await
        .map_err(|e| {
            error!("Failed to create session: {e:#}");
            map_session_init_error(&e, &config.codex_home)
        })?;
        let conversation_id = session.conversation_id;

        // This task will run until Op::Shutdown is received.
        tokio::spawn(submission_loop(session, config, rx_sub));
        let codex = Codex {
            next_id: AtomicU64::new(0),
            tx_sub,
            rx_event,
        };

        Ok(CodexSpawnOk {
            codex,
            conversation_id,
        })
    }

    /// Submit the `op` wrapped in a `Submission` with a unique ID.
    pub async fn submit(&self, op: Op) -> CodexResult<String> {
        let id = self
            .next_id
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            .to_string();
        let sub = Submission { id: id.clone(), op };
        self.submit_with_id(sub).await?;
        Ok(id)
    }

    /// Use sparingly: prefer `submit()` so Codex is responsible for generating
    /// unique IDs for each submission.
    pub async fn submit_with_id(&self, sub: Submission) -> CodexResult<()> {
        self.tx_sub
            .send(sub)
            .await
            .map_err(|_| CodexErr::InternalAgentDied)?;
        Ok(())
    }

    pub async fn next_event(&self) -> CodexResult<Event> {
        let event = self
            .rx_event
            .recv()
            .await
            .map_err(|_| CodexErr::InternalAgentDied)?;
        Ok(event)
    }
}

/// Context for an initialized model agent
///
/// A session has at most 1 running task at a time, and can be interrupted by user input.
pub(crate) struct Session {
    conversation_id: ConversationId,
    tx_event: Sender<Event>,
    state: Mutex<SessionState>,
    /// The set of enabled features should be invariant for the lifetime of the
    /// session.
    features: Features,
    pub(crate) active_turn: Mutex<Option<ActiveTurn>>,
    pub(crate) services: SessionServices,
    next_internal_sub_id: AtomicU64,
}

/// The context needed for a single turn of the conversation.
#[derive(Debug)]
pub(crate) struct TurnContext {
    pub(crate) sub_id: String,
    pub(crate) client: ModelClient,
    /// The session's current working directory. All relative paths provided by
    /// the model as well as sandbox policies are resolved against this path
    /// instead of `std::env::current_dir()`.
    pub(crate) cwd: PathBuf,
    pub(crate) developer_instructions: Option<String>,
    pub(crate) base_instructions: Option<String>,
    pub(crate) compact_prompt: Option<String>,
    pub(crate) user_instructions: Option<String>,
    pub(crate) approval_policy: AskForApproval,
    pub(crate) sandbox_policy: SandboxPolicy,
    pub(crate) shell_environment_policy: ShellEnvironmentPolicy,
    pub(crate) tools_config: ToolsConfig,
    pub(crate) ghost_snapshot: GhostSnapshotConfig,
    pub(crate) final_output_json_schema: Option<Value>,
    pub(crate) codex_linux_sandbox_exe: Option<PathBuf>,
    pub(crate) tool_call_gate: Arc<ReadinessFlag>,
    pub(crate) truncation_policy: TruncationPolicy,
}

impl TurnContext {
    pub(crate) fn resolve_path(&self, path: Option<String>) -> PathBuf {
        path.as_ref()
            .map(PathBuf::from)
            .map_or_else(|| self.cwd.clone(), |p| self.cwd.join(p))
    }

    pub(crate) fn compact_prompt(&self) -> &str {
        self.compact_prompt
            .as_deref()
            .unwrap_or(compact::SUMMARIZATION_PROMPT)
    }
}

#[derive(Clone)]
pub(crate) struct SessionConfiguration {
    /// Provider identifier ("openai", "openrouter", ...).
    provider: ModelProviderInfo,

    /// If not specified, server will use its default model.
    model: String,

    model_reasoning_effort: Option<ReasoningEffortConfig>,
    model_reasoning_summary: ReasoningSummaryConfig,

    /// Developer instructions that supplement the base instructions.
    developer_instructions: Option<String>,

    /// Model instructions that are appended to the base instructions.
    user_instructions: Option<String>,

    /// Base instructions override.
    base_instructions: Option<String>,

    /// Compact prompt override.
    compact_prompt: Option<String>,

    /// When to escalate for approval for execution
    approval_policy: Constrained<AskForApproval>,
    /// How to sandbox commands executed in the system
    sandbox_policy: Constrained<SandboxPolicy>,

    /// Working directory that should be treated as the *root* of the
    /// session. All relative paths supplied by the model as well as the
    /// execution sandbox are resolved against this directory **instead**
    /// of the process-wide current working directory. CLI front-ends are
    /// expected to expand this to an absolute path before sending the
    /// `ConfigureSession` operation so that the business-logic layer can
    /// operate deterministically.
    cwd: PathBuf,

    // TODO(pakrym): Remove config from here
    original_config_do_not_use: Arc<Config>,
    /// Source of the session (cli, vscode, exec, mcp, ...)
    session_source: SessionSource,
}

impl SessionConfiguration {
    pub(crate) fn apply(&self, updates: &SessionSettingsUpdate) -> ConstraintResult<Self> {
        let mut next_configuration = self.clone();
        if let Some(model) = updates.model.clone() {
            next_configuration.model = model;
        }
        if let Some(effort) = updates.reasoning_effort {
            next_configuration.model_reasoning_effort = effort;
        }
        if let Some(summary) = updates.reasoning_summary {
            next_configuration.model_reasoning_summary = summary;
        }
        if let Some(approval_policy) = updates.approval_policy {
            next_configuration.approval_policy.set(approval_policy)?;
        }
        if let Some(sandbox_policy) = updates.sandbox_policy.clone() {
            next_configuration.sandbox_policy.set(sandbox_policy)?;
        }
        if let Some(cwd) = updates.cwd.clone() {
            next_configuration.cwd = cwd;
        }
        Ok(next_configuration)
    }
}

#[derive(Default, Clone)]
pub(crate) struct SessionSettingsUpdate {
    pub(crate) cwd: Option<PathBuf>,
    pub(crate) approval_policy: Option<AskForApproval>,
    pub(crate) sandbox_policy: Option<SandboxPolicy>,
    pub(crate) model: Option<String>,
    pub(crate) reasoning_effort: Option<Option<ReasoningEffortConfig>>,
    pub(crate) reasoning_summary: Option<ReasoningSummaryConfig>,
    pub(crate) final_output_json_schema: Option<Option<Value>>,
}

impl Session {
    /// Don't expand the number of mutated arguments on config. We are in the process of getting rid of it.
    fn build_per_turn_config(session_configuration: &SessionConfiguration) -> Config {
        // todo(aibrahim): store this state somewhere else so we don't need to mut config
        let config = session_configuration.original_config_do_not_use.clone();
        let mut per_turn_config = (*config).clone();
        per_turn_config.model_reasoning_effort = session_configuration.model_reasoning_effort;
        per_turn_config.model_reasoning_summary = session_configuration.model_reasoning_summary;
        per_turn_config.features = config.features.clone();
        per_turn_config
    }

    #[allow(clippy::too_many_arguments)]
    fn make_turn_context(
        auth_manager: Option<Arc<AuthManager>>,
        otel_manager: &OtelManager,
        provider: ModelProviderInfo,
        session_configuration: &SessionConfiguration,
        per_turn_config: Config,
        model_family: ModelFamily,
        conversation_id: ConversationId,
        sub_id: String,
    ) -> TurnContext {
        let otel_manager = otel_manager.clone().with_model(
            session_configuration.model.as_str(),
            model_family.get_model_slug(),
        );

        let per_turn_config = Arc::new(per_turn_config);
        let client = ModelClient::new(
            per_turn_config.clone(),
            auth_manager,
            model_family.clone(),
            otel_manager,
            provider,
            session_configuration.model_reasoning_effort,
            session_configuration.model_reasoning_summary,
            conversation_id,
            session_configuration.session_source.clone(),
        );

        let tools_config = ToolsConfig::new(&ToolsConfigParams {
            model_family: &model_family,
            features: &per_turn_config.features,
        });

        TurnContext {
            sub_id,
            client,
            cwd: session_configuration.cwd.clone(),
            developer_instructions: session_configuration.developer_instructions.clone(),
            base_instructions: session_configuration.base_instructions.clone(),
            compact_prompt: session_configuration.compact_prompt.clone(),
            user_instructions: session_configuration.user_instructions.clone(),
            approval_policy: session_configuration.approval_policy.value(),
            sandbox_policy: session_configuration.sandbox_policy.get().clone(),
            shell_environment_policy: per_turn_config.shell_environment_policy.clone(),
            tools_config,
            ghost_snapshot: per_turn_config.ghost_snapshot.clone(),
            final_output_json_schema: None,
            codex_linux_sandbox_exe: per_turn_config.codex_linux_sandbox_exe.clone(),
            tool_call_gate: Arc::new(ReadinessFlag::new()),
            truncation_policy: TruncationPolicy::new(
                per_turn_config.as_ref(),
                model_family.truncation_policy,
            ),
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn new(
        session_configuration: SessionConfiguration,
        config: Arc<Config>,
        auth_manager: Arc<AuthManager>,
        models_manager: Arc<ModelsManager>,
        exec_policy: ExecPolicyManager,
        tx_event: Sender<Event>,
        initial_history: InitialHistory,
        session_source: SessionSource,
        skills_manager: Arc<SkillsManager>,
    ) -> anyhow::Result<Arc<Self>> {
        debug!(
            "Configuring session: model={}; provider={:?}",
            session_configuration.model, session_configuration.provider
        );
        if !session_configuration.cwd.is_absolute() {
            return Err(anyhow::anyhow!(
                "cwd is not absolute: {:?}",
                session_configuration.cwd
            ));
        }

        let (conversation_id, rollout_params) = match &initial_history {
            InitialHistory::New | InitialHistory::Forked(_) => {
                let conversation_id = ConversationId::default();
                (
                    conversation_id,
                    RolloutRecorderParams::new(
                        conversation_id,
                        session_configuration.user_instructions.clone(),
                        session_source,
                    ),
                )
            }
            InitialHistory::Resumed(resumed_history) => (
                resumed_history.conversation_id,
                RolloutRecorderParams::resume(resumed_history.rollout_path.clone()),
            ),
        };

        // Kick off independent async setup tasks in parallel to reduce startup latency.
        //
        // - initialize RolloutRecorder with new or resumed session info
        // - perform default shell discovery
        // - load history metadata
        let rollout_fut = RolloutRecorder::new(&config, rollout_params);

        let history_meta_fut = crate::message_history::history_metadata(&config);
        let auth_statuses_fut = compute_auth_statuses(
            config.mcp_servers.iter(),
            config.mcp_oauth_credentials_store_mode,
        );

        // Join all independent futures.
        let (rollout_recorder, (history_log_id, history_entry_count), auth_statuses) =
            tokio::join!(rollout_fut, history_meta_fut, auth_statuses_fut);

        let rollout_recorder = rollout_recorder.map_err(|e| {
            error!("failed to initialize rollout recorder: {e:#}");
            anyhow::Error::from(e)
        })?;
        let rollout_path = rollout_recorder.rollout_path.clone();

        let mut post_session_configured_events = Vec::<Event>::new();

        for (alias, feature) in config.features.legacy_feature_usages() {
            let canonical = feature.key();
            let summary = format!("`{alias}` is deprecated. Use `[features].{canonical}` instead.");
            let details = if alias == canonical {
                None
            } else {
                Some(format!(
                    "Enable it with `--enable {canonical}` or `[features].{canonical}` in config.toml. See https://github.com/openai/codex/blob/main/docs/config.md#feature-flags for details."
                ))
            };
            post_session_configured_events.push(Event {
                id: INITIAL_SUBMIT_ID.to_owned(),
                msg: EventMsg::DeprecationNotice(DeprecationNoticeEvent { summary, details }),
            });
        }
        maybe_push_chat_wire_api_deprecation(&config, &mut post_session_configured_events);

        // todo(aibrahim): why are we passing model here while it can change?
        let otel_manager = OtelManager::new(
            conversation_id,
            session_configuration.model.as_str(),
            session_configuration.model.as_str(),
            auth_manager.auth().and_then(|a| a.get_account_id()),
            auth_manager.auth().and_then(|a| a.get_account_email()),
            auth_manager.auth().map(|a| a.mode),
            config.otel.log_user_prompt,
            terminal::user_agent(),
            session_configuration.session_source.clone(),
        );

        otel_manager.conversation_starts(
            config.model_provider.name.as_str(),
            config.model_reasoning_effort,
            config.model_reasoning_summary,
            config.model_context_window,
            config.model_auto_compact_token_limit,
            config.approval_policy.value(),
            config.sandbox_policy.get().clone(),
            config.mcp_servers.keys().map(String::as_str).collect(),
            config.active_profile.clone(),
        );

        let mut default_shell = shell::default_user_shell();
        // Create the mutable state for the Session.
        if config.features.enabled(Feature::ShellSnapshot) {
            default_shell.shell_snapshot =
                ShellSnapshot::try_new(&config.codex_home, &default_shell)
                    .await
                    .map(Arc::new);
        }
        let state = SessionState::new(session_configuration.clone());

        let services = SessionServices {
            mcp_connection_manager: Arc::new(RwLock::new(McpConnectionManager::default())),
            mcp_startup_cancellation_token: CancellationToken::new(),
            unified_exec_manager: UnifiedExecSessionManager::default(),
            notifier: UserNotifier::new(config.notify.clone()),
            rollout: Mutex::new(Some(rollout_recorder)),
            user_shell: Arc::new(default_shell),
            show_raw_agent_reasoning: config.show_raw_agent_reasoning,
            exec_policy,
            auth_manager: Arc::clone(&auth_manager),
            otel_manager,
            models_manager: Arc::clone(&models_manager),
            tool_approvals: Mutex::new(ApprovalStore::default()),
            skills_manager,
        };

        let sess = Arc::new(Session {
            conversation_id,
            tx_event: tx_event.clone(),
            state: Mutex::new(state),
            features: config.features.clone(),
            active_turn: Mutex::new(None),
            services,
            next_internal_sub_id: AtomicU64::new(0),
        });

        // Dispatch the SessionConfiguredEvent first and then report any errors.
        // If resuming, include converted initial messages in the payload so UIs can render them immediately.
        let initial_messages = initial_history.get_event_msgs();
        let events = std::iter::once(Event {
            id: INITIAL_SUBMIT_ID.to_owned(),
            msg: EventMsg::SessionConfigured(SessionConfiguredEvent {
                session_id: conversation_id,
                model: session_configuration.model.clone(),
                model_provider_id: config.model_provider_id.clone(),
                approval_policy: session_configuration.approval_policy.value(),
                sandbox_policy: session_configuration.sandbox_policy.get().clone(),
                cwd: session_configuration.cwd.clone(),
                reasoning_effort: session_configuration.model_reasoning_effort,
                history_log_id,
                history_entry_count,
                initial_messages,
                rollout_path,
            }),
        })
        .chain(post_session_configured_events.into_iter());
        for event in events {
            sess.send_event_raw(event).await;
        }

        // Construct sandbox_state before initialize() so it can be sent to each
        // MCP server immediately after it becomes ready (avoiding blocking).
        let sandbox_state = SandboxState {
            sandbox_policy: session_configuration.sandbox_policy.get().clone(),
            codex_linux_sandbox_exe: config.codex_linux_sandbox_exe.clone(),
            sandbox_cwd: session_configuration.cwd.clone(),
        };
        sess.services
            .mcp_connection_manager
            .write()
            .await
            .initialize(
                config.mcp_servers.clone(),
                config.mcp_oauth_credentials_store_mode,
                auth_statuses.clone(),
                tx_event.clone(),
                sess.services.mcp_startup_cancellation_token.clone(),
                sandbox_state,
            )
            .await;

        // record_initial_history can emit events. We record only after the SessionConfiguredEvent is emitted.
        sess.record_initial_history(initial_history).await;

        Ok(sess)
    }

    pub(crate) fn get_tx_event(&self) -> Sender<Event> {
        self.tx_event.clone()
    }

    /// Ensure all rollout writes are durably flushed.
    pub(crate) async fn flush_rollout(&self) {
        let recorder = {
            let guard = self.services.rollout.lock().await;
            guard.clone()
        };
        if let Some(rec) = recorder
            && let Err(e) = rec.flush().await
        {
            warn!("failed to flush rollout recorder: {e}");
        }
    }

    fn next_internal_sub_id(&self) -> String {
        let id = self
            .next_internal_sub_id
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        format!("auto-compact-{id}")
    }

    async fn get_total_token_usage(&self) -> i64 {
        let state = self.state.lock().await;
        state.get_total_token_usage()
    }

    async fn record_initial_history(&self, conversation_history: InitialHistory) {
        let turn_context = self.new_default_turn().await;
        match conversation_history {
            InitialHistory::New => {
                // Build and record initial items (user instructions + environment context)
                let items = self.build_initial_context(&turn_context);
                self.record_conversation_items(&turn_context, &items).await;
                // Ensure initial items are visible to immediate readers (e.g., tests, forks).
                self.flush_rollout().await;
            }
            InitialHistory::Resumed(_) | InitialHistory::Forked(_) => {
                let rollout_items = conversation_history.get_rollout_items();
                let persist = matches!(conversation_history, InitialHistory::Forked(_));

                // If resuming, warn when the last recorded model differs from the current one.
                if let InitialHistory::Resumed(_) = conversation_history
                    && let Some(prev) = rollout_items.iter().rev().find_map(|it| {
                        if let RolloutItem::TurnContext(ctx) = it {
                            Some(ctx.model.as_str())
                        } else {
                            None
                        }
                    })
                {
                    let curr = turn_context.client.get_model();
                    if prev != curr {
                        warn!(
                            "resuming session with different model: previous={prev}, current={curr}"
                        );
                        self.send_event(
                            &turn_context,
                            EventMsg::Warning(WarningEvent {
                                message: format!(
                                    "This session was recorded with model `{prev}` but is resuming with `{curr}`. \
                         Consider switching back to `{prev}` as it may affect Codex performance."
                                ),
                            }),
                        )
                            .await;
                    }
                }

                // Always add response items to conversation history
                let reconstructed_history =
                    self.reconstruct_history_from_rollout(&turn_context, &rollout_items);
                if !reconstructed_history.is_empty() {
                    self.record_into_history(&reconstructed_history, &turn_context)
                        .await;
                }

                // If persisting, persist all rollout items as-is (recorder filters)
                if persist && !rollout_items.is_empty() {
                    self.persist_rollout_items(&rollout_items).await;
                }
                // Flush after seeding history and any persisted rollout copy.
                self.flush_rollout().await;
            }
        }
    }

    pub(crate) async fn update_settings(
        &self,
        updates: SessionSettingsUpdate,
    ) -> ConstraintResult<()> {
        let mut state = self.state.lock().await;

        match state.session_configuration.apply(&updates) {
            Ok(updated) => {
                state.session_configuration = updated;
                Ok(())
            }
            Err(err) => {
                warn!("rejected session settings update: {err}");
                Err(err)
            }
        }
    }

    pub(crate) async fn new_turn_with_sub_id(
        &self,
        sub_id: String,
        updates: SessionSettingsUpdate,
    ) -> ConstraintResult<Arc<TurnContext>> {
        let (session_configuration, sandbox_policy_changed) = {
            let mut state = self.state.lock().await;
            match state.session_configuration.clone().apply(&updates) {
                Ok(next) => {
                    let sandbox_policy_changed =
                        state.session_configuration.sandbox_policy != next.sandbox_policy;
                    state.session_configuration = next.clone();
                    (next, sandbox_policy_changed)
                }
                Err(err) => {
                    drop(state);
                    self.send_event_raw(Event {
                        id: sub_id.clone(),
                        msg: EventMsg::Error(ErrorEvent {
                            message: err.to_string(),
                            codex_error_info: Some(CodexErrorInfo::BadRequest),
                        }),
                    })
                    .await;
                    return Err(err);
                }
            }
        };

        Ok(self
            .new_turn_from_configuration(
                sub_id,
                session_configuration,
                updates.final_output_json_schema,
                sandbox_policy_changed,
            )
            .await)
    }

    async fn new_turn_from_configuration(
        &self,
        sub_id: String,
        session_configuration: SessionConfiguration,
        final_output_json_schema: Option<Option<Value>>,
        sandbox_policy_changed: bool,
    ) -> Arc<TurnContext> {
        let per_turn_config = Self::build_per_turn_config(&session_configuration);

        if sandbox_policy_changed {
            let sandbox_state = SandboxState {
                sandbox_policy: per_turn_config.sandbox_policy.get().clone(),
                codex_linux_sandbox_exe: per_turn_config.codex_linux_sandbox_exe.clone(),
                sandbox_cwd: per_turn_config.cwd.clone(),
            };
            if let Err(e) = self
                .services
                .mcp_connection_manager
                .read()
                .await
                .notify_sandbox_state_change(&sandbox_state)
                .await
            {
                warn!("Failed to notify sandbox state change to MCP servers: {e:#}");
            }
        }

        let model_family = self
            .services
            .models_manager
            .construct_model_family(session_configuration.model.as_str(), &per_turn_config)
            .await;
        let mut turn_context: TurnContext = Self::make_turn_context(
            Some(Arc::clone(&self.services.auth_manager)),
            &self.services.otel_manager,
            session_configuration.provider.clone(),
            &session_configuration,
            per_turn_config,
            model_family,
            self.conversation_id,
            sub_id,
        );
        if let Some(final_schema) = final_output_json_schema {
            turn_context.final_output_json_schema = final_schema;
        }
        Arc::new(turn_context)
    }

    pub(crate) async fn new_default_turn(&self) -> Arc<TurnContext> {
        self.new_default_turn_with_sub_id(self.next_internal_sub_id())
            .await
    }

    pub(crate) async fn new_default_turn_with_sub_id(&self, sub_id: String) -> Arc<TurnContext> {
        let session_configuration = {
            let state = self.state.lock().await;
            state.session_configuration.clone()
        };
        self.new_turn_from_configuration(sub_id, session_configuration, None, false)
            .await
    }

    fn build_environment_update_item(
        &self,
        previous: Option<&Arc<TurnContext>>,
        next: &TurnContext,
    ) -> Option<ResponseItem> {
        let prev = previous?;

        let shell = self.user_shell();
        let prev_context = EnvironmentContext::from_turn_context(prev.as_ref(), shell.as_ref());
        let next_context = EnvironmentContext::from_turn_context(next, shell.as_ref());
        if prev_context.equals_except_shell(&next_context) {
            return None;
        }
        Some(ResponseItem::from(EnvironmentContext::diff(
            prev.as_ref(),
            next,
            shell.as_ref(),
        )))
    }

    /// 将事件持久化到 rollout，并发送给前端/客户端事件订阅者。
    pub(crate) async fn send_event(&self, turn_context: &TurnContext, msg: EventMsg) {
        // 先把原始事件留一份，用于后续派生/兼容旧版（legacy）事件。
        let legacy_source = msg.clone();
        // 把 EventMsg 包装成带 turn_id(sub_id) 的 Event，便于前端按 turn 归属消费/渲染。
        let event = Event {
            id: turn_context.sub_id.clone(),
            msg,
        };
        // 发送“新格式”事件：内部会写入 rollout（持久化记录），并推送到事件通道给前端消费。
        self.send_event_raw(event).await;

        // 是否把 agent 的原始推理过程（raw reasoning）也暴露给前端；会影响 legacy 事件的生成内容。
        let show_raw_agent_reasoning = self.show_raw_agent_reasoning();
        // 将同一个事件按需要展开成一组旧版事件（为了兼容旧前端/旧协议的事件类型）。
        for legacy in legacy_source.as_legacy_events(show_raw_agent_reasoning) {
            // legacy 事件同样挂在当前 turn 的 sub_id 下，保证前端看到的 turn 关联一致。
            let legacy_event = Event {
                id: turn_context.sub_id.clone(),
                msg: legacy,
            };
            // 逐个发送 legacy 事件（同样会持久化 + 推送到事件通道）。
            self.send_event_raw(legacy_event).await;
        }
    }

    pub(crate) async fn send_event_raw(&self, event: Event) {
        // Persist the event into rollout (recorder filters as needed)
        let rollout_items = vec![RolloutItem::EventMsg(event.msg.clone())];
        self.persist_rollout_items(&rollout_items).await;
        if let Err(e) = self.tx_event.send(event).await {
            error!("failed to send tool call event: {e}");
        }
    }

    pub(crate) async fn emit_turn_item_started(&self, turn_context: &TurnContext, item: &TurnItem) {
        self.send_event(
            turn_context,
            EventMsg::ItemStarted(ItemStartedEvent {
                thread_id: self.conversation_id,
                turn_id: turn_context.sub_id.clone(),
                item: item.clone(),
            }),
        )
        .await;
    }

    pub(crate) async fn emit_turn_item_completed(
        &self,
        turn_context: &TurnContext,
        item: TurnItem,
    ) {
        self.send_event(
            turn_context,
            EventMsg::ItemCompleted(ItemCompletedEvent {
                thread_id: self.conversation_id,
                turn_id: turn_context.sub_id.clone(),
                item,
            }),
        )
        .await;
    }

    /// Adds an execpolicy amendment to both the in-memory and on-disk policies so future
    /// commands can use the newly approved prefix.
    pub(crate) async fn persist_execpolicy_amendment(
        &self,
        amendment: &ExecPolicyAmendment,
    ) -> Result<(), ExecPolicyUpdateError> {
        let features = self.features.clone();
        let codex_home = self
            .state
            .lock()
            .await
            .session_configuration
            .original_config_do_not_use
            .codex_home
            .clone();

        if !features.enabled(Feature::ExecPolicy) {
            error!("attempted to append execpolicy rule while execpolicy feature is disabled");
            return Err(ExecPolicyUpdateError::FeatureDisabled);
        }

        self.services
            .exec_policy
            .append_amendment_and_update(&codex_home, amendment)
            .await?;

        Ok(())
    }

    /// Emit an exec approval request event and await the user's decision.
    ///
    /// The request is keyed by `sub_id`/`call_id` so matching responses are delivered
    /// to the correct in-flight turn. If the task is aborted, this returns the
    /// default `ReviewDecision` (`Denied`).
    #[allow(clippy::too_many_arguments)]
    pub async fn request_command_approval(
        &self,
        turn_context: &TurnContext,
        call_id: String,
        command: Vec<String>,
        cwd: PathBuf,
        reason: Option<String>,
        proposed_execpolicy_amendment: Option<ExecPolicyAmendment>,
    ) -> ReviewDecision {
        let sub_id = turn_context.sub_id.clone();
        // Add the tx_approve callback to the map before sending the request.
        let (tx_approve, rx_approve) = oneshot::channel();
        let event_id = sub_id.clone();
        let prev_entry = {
            let mut active = self.active_turn.lock().await;
            match active.as_mut() {
                Some(at) => {
                    let mut ts = at.turn_state.lock().await;
                    ts.insert_pending_approval(sub_id, tx_approve)
                }
                None => None,
            }
        };
        if prev_entry.is_some() {
            warn!("Overwriting existing pending approval for sub_id: {event_id}");
        }

        let parsed_cmd = parse_command(&command);
        let event = EventMsg::ExecApprovalRequest(ExecApprovalRequestEvent {
            call_id,
            turn_id: turn_context.sub_id.clone(),
            command,
            cwd,
            reason,
            proposed_execpolicy_amendment,
            parsed_cmd,
        });
        self.send_event(turn_context, event).await;
        rx_approve.await.unwrap_or_default()
    }

    pub async fn request_patch_approval(
        &self,
        turn_context: &TurnContext,
        call_id: String,
        changes: HashMap<PathBuf, FileChange>,
        reason: Option<String>,
        grant_root: Option<PathBuf>,
    ) -> oneshot::Receiver<ReviewDecision> {
        let sub_id = turn_context.sub_id.clone();
        // Add the tx_approve callback to the map before sending the request.
        let (tx_approve, rx_approve) = oneshot::channel();
        let event_id = sub_id.clone();
        let prev_entry = {
            let mut active = self.active_turn.lock().await;
            match active.as_mut() {
                Some(at) => {
                    let mut ts = at.turn_state.lock().await;
                    ts.insert_pending_approval(sub_id, tx_approve)
                }
                None => None,
            }
        };
        if prev_entry.is_some() {
            warn!("Overwriting existing pending approval for sub_id: {event_id}");
        }

        let event = EventMsg::ApplyPatchApprovalRequest(ApplyPatchApprovalRequestEvent {
            call_id,
            turn_id: turn_context.sub_id.clone(),
            changes,
            reason,
            grant_root,
        });
        self.send_event(turn_context, event).await;
        rx_approve
    }

    pub async fn notify_approval(&self, sub_id: &str, decision: ReviewDecision) {
        let entry = {
            let mut active = self.active_turn.lock().await;
            match active.as_mut() {
                Some(at) => {
                    let mut ts = at.turn_state.lock().await;
                    ts.remove_pending_approval(sub_id)
                }
                None => None,
            }
        };
        match entry {
            Some(tx_approve) => {
                tx_approve.send(decision).ok();
            }
            None => {
                warn!("No pending approval found for sub_id: {sub_id}");
            }
        }
    }

    pub async fn resolve_elicitation(
        &self,
        server_name: String,
        id: RequestId,
        response: ElicitationResponse,
    ) -> anyhow::Result<()> {
        self.services
            .mcp_connection_manager
            .read()
            .await
            .resolve_elicitation(server_name, id, response)
            .await
    }

    /// Records input items: always append to conversation history and
    /// persist these response items to rollout.
    pub(crate) async fn record_conversation_items(
        &self,
        turn_context: &TurnContext,
        items: &[ResponseItem],
    ) {
        self.record_into_history(items, turn_context).await;
        self.persist_rollout_response_items(items).await;
        self.send_raw_response_items(turn_context, items).await;
    }

    fn reconstruct_history_from_rollout(
        &self,
        turn_context: &TurnContext,
        rollout_items: &[RolloutItem],
    ) -> Vec<ResponseItem> {
        let mut history = ContextManager::new();
        for item in rollout_items {
            match item {
                RolloutItem::ResponseItem(response_item) => {
                    history.record_items(
                        std::iter::once(response_item),
                        turn_context.truncation_policy,
                    );
                }
                RolloutItem::Compacted(compacted) => {
                    let snapshot = history.get_history();
                    // TODO(jif) clean
                    if let Some(replacement) = &compacted.replacement_history {
                        history.replace(replacement.clone());
                    } else {
                        let user_messages = collect_user_messages(&snapshot);
                        let rebuilt = compact::build_compacted_history(
                            self.build_initial_context(turn_context),
                            &user_messages,
                            &compacted.message,
                        );
                        history.replace(rebuilt);
                    }
                }
                _ => {}
            }
        }
        history.get_history()
    }

    /// Append ResponseItems to the in-memory conversation history only.
    pub(crate) async fn record_into_history(
        &self,
        items: &[ResponseItem],
        turn_context: &TurnContext,
    ) {
        let mut state = self.state.lock().await;
        state.record_items(items.iter(), turn_context.truncation_policy);
    }

    pub(crate) async fn record_model_warning(&self, message: impl Into<String>, ctx: &TurnContext) {
        if !self.enabled(Feature::ModelWarnings) {
            return;
        }

        let item = ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: format!("Warning: {}", message.into()),
            }],
        };

        self.record_conversation_items(ctx, &[item]).await;
    }

    pub(crate) async fn replace_history(&self, items: Vec<ResponseItem>) {
        let mut state = self.state.lock().await;
        state.replace_history(items);
    }

    async fn persist_rollout_response_items(&self, items: &[ResponseItem]) {
        let rollout_items: Vec<RolloutItem> = items
            .iter()
            .cloned()
            .map(RolloutItem::ResponseItem)
            .collect();
        self.persist_rollout_items(&rollout_items).await;
    }

    pub fn enabled(&self, feature: Feature) -> bool {
        self.features.enabled(feature)
    }

    pub(crate) fn features(&self) -> Features {
        self.features.clone()
    }

    async fn send_raw_response_items(&self, turn_context: &TurnContext, items: &[ResponseItem]) {
        for item in items {
            self.send_event(
                turn_context,
                EventMsg::RawResponseItem(RawResponseItemEvent { item: item.clone() }),
            )
            .await;
        }
    }

    pub(crate) fn build_initial_context(&self, turn_context: &TurnContext) -> Vec<ResponseItem> {
        let mut items = Vec::<ResponseItem>::with_capacity(3);
        let shell = self.user_shell();
        if let Some(developer_instructions) = turn_context.developer_instructions.as_deref() {
            items.push(DeveloperInstructions::new(developer_instructions.to_string()).into());
        }
        if let Some(user_instructions) = turn_context.user_instructions.as_deref() {
            items.push(
                UserInstructions {
                    text: user_instructions.to_string(),
                    directory: turn_context.cwd.to_string_lossy().into_owned(),
                }
                .into(),
            );
        }
        items.push(ResponseItem::from(EnvironmentContext::new(
            Some(turn_context.cwd.clone()),
            Some(turn_context.approval_policy),
            Some(turn_context.sandbox_policy.clone()),
            shell.as_ref().clone(),
        )));
        items
    }

    pub(crate) async fn persist_rollout_items(&self, items: &[RolloutItem]) {
        let recorder = {
            let guard = self.services.rollout.lock().await;
            guard.clone()
        };
        if let Some(rec) = recorder
            && let Err(e) = rec.record_items(items).await
        {
            error!("failed to record rollout items: {e:#}");
        }
    }

    pub(crate) async fn clone_history(&self) -> ContextManager {
        let state = self.state.lock().await;
        state.clone_history()
    }

    pub(crate) async fn update_token_usage_info(
        &self,
        turn_context: &TurnContext,
        token_usage: Option<&TokenUsage>,
    ) {
        {
            let mut state = self.state.lock().await;
            if let Some(token_usage) = token_usage {
                state.update_token_info_from_usage(
                    token_usage,
                    turn_context.client.get_model_context_window(),
                );
            }
        }
        self.send_token_count_event(turn_context).await;
    }

    pub(crate) async fn recompute_token_usage(&self, turn_context: &TurnContext) {
        let Some(estimated_total_tokens) = self
            .clone_history()
            .await
            .estimate_token_count(turn_context)
        else {
            return;
        };
        {
            let mut state = self.state.lock().await;
            let mut info = state.token_info().unwrap_or(TokenUsageInfo {
                total_token_usage: TokenUsage::default(),
                last_token_usage: TokenUsage::default(),
                model_context_window: None,
            });

            info.last_token_usage = TokenUsage {
                input_tokens: 0,
                cached_input_tokens: 0,
                output_tokens: 0,
                reasoning_output_tokens: 0,
                total_tokens: estimated_total_tokens.max(0),
            };

            if info.model_context_window.is_none() {
                info.model_context_window = turn_context.client.get_model_context_window();
            }

            state.set_token_info(Some(info));
        }
        self.send_token_count_event(turn_context).await;
    }

    pub(crate) async fn update_rate_limits(
        &self,
        turn_context: &TurnContext,
        new_rate_limits: RateLimitSnapshot,
    ) {
        {
            let mut state = self.state.lock().await;
            state.set_rate_limits(new_rate_limits);
        }
        self.send_token_count_event(turn_context).await;
    }

    async fn send_token_count_event(&self, turn_context: &TurnContext) {
        let (info, rate_limits) = {
            let state = self.state.lock().await;
            state.token_info_and_rate_limits()
        };
        let event = EventMsg::TokenCount(TokenCountEvent { info, rate_limits });
        self.send_event(turn_context, event).await;
    }

    pub(crate) async fn set_total_tokens_full(&self, turn_context: &TurnContext) {
        let context_window = turn_context.client.get_model_context_window();
        if let Some(context_window) = context_window {
            {
                let mut state = self.state.lock().await;
                state.set_token_usage_full(context_window);
            }
            self.send_token_count_event(turn_context).await;
        }
    }

    pub(crate) async fn record_response_item_and_emit_turn_item(
        &self,
        turn_context: &TurnContext,
        response_item: ResponseItem,
    ) {
        // Add to conversation history and persist response item to rollout.
        self.record_conversation_items(turn_context, std::slice::from_ref(&response_item))
            .await;

        // Derive a turn item and emit lifecycle events if applicable.
        if let Some(item) = parse_turn_item(&response_item) {
            self.emit_turn_item_started(turn_context, &item).await;
            self.emit_turn_item_completed(turn_context, item).await;
        }
    }

    pub(crate) async fn notify_background_event(
        &self,
        turn_context: &TurnContext,
        message: impl Into<String>,
    ) {
        let event = EventMsg::BackgroundEvent(BackgroundEventEvent {
            message: message.into(),
        });
        self.send_event(turn_context, event).await;
    }

    pub(crate) async fn notify_stream_error(
        &self,
        turn_context: &TurnContext,
        message: impl Into<String>,
        codex_error: CodexErr,
    ) {
        let additional_details = codex_error.to_string();
        let codex_error_info = CodexErrorInfo::ResponseStreamDisconnected {
            http_status_code: codex_error.http_status_code_value(),
        };
        let event = EventMsg::StreamError(StreamErrorEvent {
            message: message.into(),
            codex_error_info: Some(codex_error_info),
            additional_details: Some(additional_details),
        });
        self.send_event(turn_context, event).await;
    }

    async fn maybe_start_ghost_snapshot(
        self: &Arc<Self>,
        turn_context: Arc<TurnContext>,
        cancellation_token: CancellationToken,
    ) {
        if !self.enabled(Feature::GhostCommit) {
            return;
        }
        let token = match turn_context.tool_call_gate.subscribe().await {
            Ok(token) => token,
            Err(err) => {
                warn!("failed to subscribe to ghost snapshot readiness: {err}");
                return;
            }
        };

        info!("spawning ghost snapshot task");
        let task = GhostSnapshotTask::new(token);
        Arc::new(task)
            .run(
                Arc::new(SessionTaskContext::new(self.clone())),
                turn_context.clone(),
                Vec::new(),
                cancellation_token,
            )
            .await;
    }

    /// Returns the input if there was no task running to inject into
    pub async fn inject_input(&self, input: Vec<UserInput>) -> Result<(), Vec<UserInput>> {
        let mut active = self.active_turn.lock().await;
        match active.as_mut() {
            Some(at) => {
                let mut ts = at.turn_state.lock().await;
                ts.push_pending_input(input.into());
                Ok(())
            }
            None => Err(input),
        }
    }

    pub async fn get_pending_input(&self) -> Vec<ResponseInputItem> {
        let mut active = self.active_turn.lock().await;
        match active.as_mut() {
            Some(at) => {
                let mut ts = at.turn_state.lock().await;
                ts.take_pending_input()
            }
            None => Vec::with_capacity(0),
        }
    }

    pub async fn list_resources(
        &self,
        server: &str,
        params: Option<ListResourcesRequestParams>,
    ) -> anyhow::Result<ListResourcesResult> {
        self.services
            .mcp_connection_manager
            .read()
            .await
            .list_resources(server, params)
            .await
    }

    pub async fn list_resource_templates(
        &self,
        server: &str,
        params: Option<ListResourceTemplatesRequestParams>,
    ) -> anyhow::Result<ListResourceTemplatesResult> {
        self.services
            .mcp_connection_manager
            .read()
            .await
            .list_resource_templates(server, params)
            .await
    }

    pub async fn read_resource(
        &self,
        server: &str,
        params: ReadResourceRequestParams,
    ) -> anyhow::Result<ReadResourceResult> {
        self.services
            .mcp_connection_manager
            .read()
            .await
            .read_resource(server, params)
            .await
    }

    pub async fn call_tool(
        &self,
        server: &str,
        tool: &str,
        arguments: Option<serde_json::Value>,
    ) -> anyhow::Result<CallToolResult> {
        self.services
            .mcp_connection_manager
            .read()
            .await
            .call_tool(server, tool, arguments)
            .await
    }

    pub(crate) async fn parse_mcp_tool_name(&self, tool_name: &str) -> Option<(String, String)> {
        self.services
            .mcp_connection_manager
            .read()
            .await
            .parse_tool_name(tool_name)
            .await
    }

    pub async fn interrupt_task(self: &Arc<Self>) {
        info!("interrupt received: abort current task, if any");
        let has_active_turn = { self.active_turn.lock().await.is_some() };
        if has_active_turn {
            self.abort_all_tasks(TurnAbortReason::Interrupted).await;
        } else {
            self.cancel_mcp_startup().await;
        }
    }

    pub(crate) fn notifier(&self) -> &UserNotifier {
        &self.services.notifier
    }

    pub(crate) fn user_shell(&self) -> Arc<shell::Shell> {
        Arc::clone(&self.services.user_shell)
    }

    fn show_raw_agent_reasoning(&self) -> bool {
        self.services.show_raw_agent_reasoning
    }

    async fn cancel_mcp_startup(&self) {
        self.services.mcp_startup_cancellation_token.cancel();
    }
}

async fn submission_loop(sess: Arc<Session>, config: Arc<Config>, rx_sub: Receiver<Submission>) {
    // Seed with context in case there is an OverrideTurnContext first.
    let mut previous_context: Option<Arc<TurnContext>> = Some(sess.new_default_turn().await);

    // To break out of this loop, send Op::Shutdown.
    while let Ok(sub) = rx_sub.recv().await {
        debug!(?sub, "Submission");
        match sub.op.clone() {
            Op::Interrupt => {
                handlers::interrupt(&sess).await;
            }
            Op::OverrideTurnContext {
                cwd,
                approval_policy,
                sandbox_policy,
                model,
                effort,
                summary,
            } => {
                handlers::override_turn_context(
                    &sess,
                    sub.id.clone(),
                    SessionSettingsUpdate {
                        cwd,
                        approval_policy,
                        sandbox_policy,
                        model,
                        reasoning_effort: effort,
                        reasoning_summary: summary,
                        ..Default::default()
                    },
                )
                .await;
            }
            Op::UserInput { .. } | Op::UserTurn { .. } => {
                handlers::user_input_or_turn(&sess, sub.id.clone(), sub.op, &mut previous_context)
                    .await;
            }
            Op::ExecApproval { id, decision } => {
                handlers::exec_approval(&sess, id, decision).await;
            }
            Op::PatchApproval { id, decision } => {
                handlers::patch_approval(&sess, id, decision).await;
            }
            Op::AddToHistory { text } => {
                handlers::add_to_history(&sess, &config, text).await;
            }
            Op::GetHistoryEntryRequest { offset, log_id } => {
                handlers::get_history_entry_request(&sess, &config, sub.id.clone(), offset, log_id)
                    .await;
            }
            Op::ListMcpTools => {
                handlers::list_mcp_tools(&sess, &config, sub.id.clone()).await;
            }
            Op::ListCustomPrompts => {
                handlers::list_custom_prompts(&sess, sub.id.clone()).await;
            }
            Op::ListSkills { cwds, force_reload } => {
                handlers::list_skills(&sess, sub.id.clone(), cwds, force_reload).await;
            }
            Op::Undo => {
                handlers::undo(&sess, sub.id.clone()).await;
            }
            Op::Compact => {
                handlers::compact(&sess, sub.id.clone()).await;
            }
            Op::RunUserShellCommand { command } => {
                handlers::run_user_shell_command(
                    &sess,
                    sub.id.clone(),
                    command,
                    &mut previous_context,
                )
                .await;
            }
            Op::ResolveElicitation {
                server_name,
                request_id,
                decision,
            } => {
                handlers::resolve_elicitation(&sess, server_name, request_id, decision).await;
            }
            Op::Shutdown => {
                if handlers::shutdown(&sess, sub.id.clone()).await {
                    break;
                }
            }
            Op::Review { review_request } => {
                handlers::review(&sess, &config, sub.id.clone(), review_request).await;
            }
            _ => {} // Ignore unknown ops; enum is non_exhaustive to allow extensions.
        }
    }
    debug!("Agent loop exited");
}

/// Operation handlers
mod handlers {
    use crate::codex::Session;
    use crate::codex::SessionSettingsUpdate;
    use crate::codex::TurnContext;

    use crate::codex::spawn_review_thread;
    use crate::config::Config;
    use crate::features::Feature;
    use crate::mcp::auth::compute_auth_statuses;
    use crate::mcp::collect_mcp_snapshot_from_manager;
    use crate::review_prompts::resolve_review_request;
    use crate::tasks::CompactTask;
    use crate::tasks::RegularTask;
    use crate::tasks::UndoTask;
    use crate::tasks::UserShellCommandTask;
    use codex_protocol::custom_prompts::CustomPrompt;
    use codex_protocol::protocol::CodexErrorInfo;
    use codex_protocol::protocol::ErrorEvent;
    use codex_protocol::protocol::Event;
    use codex_protocol::protocol::EventMsg;
    use codex_protocol::protocol::ListCustomPromptsResponseEvent;
    use codex_protocol::protocol::ListSkillsResponseEvent;
    use codex_protocol::protocol::Op;
    use codex_protocol::protocol::ReviewDecision;
    use codex_protocol::protocol::ReviewRequest;
    use codex_protocol::protocol::SkillsListEntry;
    use codex_protocol::protocol::TurnAbortReason;
    use codex_protocol::protocol::WarningEvent;

    use codex_protocol::user_input::UserInput;
    use codex_rmcp_client::ElicitationAction;
    use codex_rmcp_client::ElicitationResponse;
    use mcp_types::RequestId;
    use std::path::PathBuf;
    use std::sync::Arc;
    use tracing::info;
    use tracing::warn;

    pub async fn interrupt(sess: &Arc<Session>) {
        sess.interrupt_task().await;
    }

    pub async fn override_turn_context(
        sess: &Session,
        sub_id: String,
        updates: SessionSettingsUpdate,
    ) {
        if let Err(err) = sess.update_settings(updates).await {
            sess.send_event_raw(Event {
                id: sub_id,
                msg: EventMsg::Error(ErrorEvent {
                    message: err.to_string(),
                    codex_error_info: Some(CodexErrorInfo::BadRequest),
                }),
            })
            .await;
        }
    }

    pub async fn user_input_or_turn(
        sess: &Arc<Session>,
        sub_id: String,
        op: Op,
        previous_context: &mut Option<Arc<TurnContext>>,
    ) {
        let (items, updates) = match op {
            Op::UserTurn {
                cwd,
                approval_policy,
                sandbox_policy,
                model,
                effort,
                summary,
                final_output_json_schema,
                items,
            } => (
                items,
                SessionSettingsUpdate {
                    cwd: Some(cwd),
                    approval_policy: Some(approval_policy),
                    sandbox_policy: Some(sandbox_policy),
                    model: Some(model),
                    reasoning_effort: Some(effort),
                    reasoning_summary: Some(summary),
                    final_output_json_schema: Some(final_output_json_schema),
                },
            ),
            Op::UserInput { items } => (items, SessionSettingsUpdate::default()),
            _ => unreachable!(),
        };

        let Ok(current_context) = sess.new_turn_with_sub_id(sub_id, updates).await else {
            // new_turn_with_sub_id already emits the error event.
            return;
        };
        current_context
            .client
            .get_otel_manager()
            .user_prompt(&items);

        // Attempt to inject input into current task
        if let Err(items) = sess.inject_input(items).await {
            if let Some(env_item) =
                sess.build_environment_update_item(previous_context.as_ref(), &current_context)
            {
                sess.record_conversation_items(&current_context, std::slice::from_ref(&env_item))
                    .await;
            }

            sess.spawn_task(Arc::clone(&current_context), items, RegularTask)
                .await;
            *previous_context = Some(current_context);
        }
    }

    pub async fn run_user_shell_command(
        sess: &Arc<Session>,
        sub_id: String,
        command: String,
        previous_context: &mut Option<Arc<TurnContext>>,
    ) {
        let turn_context = sess.new_default_turn_with_sub_id(sub_id).await;
        sess.spawn_task(
            Arc::clone(&turn_context),
            Vec::new(),
            UserShellCommandTask::new(command),
        )
        .await;
        *previous_context = Some(turn_context);
    }

    pub async fn resolve_elicitation(
        sess: &Arc<Session>,
        server_name: String,
        request_id: RequestId,
        decision: codex_protocol::approvals::ElicitationAction,
    ) {
        let action = match decision {
            codex_protocol::approvals::ElicitationAction::Accept => ElicitationAction::Accept,
            codex_protocol::approvals::ElicitationAction::Decline => ElicitationAction::Decline,
            codex_protocol::approvals::ElicitationAction::Cancel => ElicitationAction::Cancel,
        };
        let response = ElicitationResponse {
            action,
            content: None,
        };
        if let Err(err) = sess
            .resolve_elicitation(server_name, request_id, response)
            .await
        {
            warn!(
                error = %err,
                "failed to resolve elicitation request in session"
            );
        }
    }

    /// Propagate a user's exec approval decision to the session.
    /// Also optionally applies an execpolicy amendment.
    pub async fn exec_approval(sess: &Arc<Session>, id: String, decision: ReviewDecision) {
        if let ReviewDecision::ApprovedExecpolicyAmendment {
            proposed_execpolicy_amendment,
        } = &decision
            && let Err(err) = sess
                .persist_execpolicy_amendment(proposed_execpolicy_amendment)
                .await
        {
            let message = format!("Failed to apply execpolicy amendment: {err}");
            tracing::warn!("{message}");
            let warning = EventMsg::Warning(WarningEvent { message });
            sess.send_event_raw(Event {
                id: id.clone(),
                msg: warning,
            })
            .await;
        }
        match decision {
            ReviewDecision::Abort => {
                sess.interrupt_task().await;
            }
            other => sess.notify_approval(&id, other).await,
        }
    }

    pub async fn patch_approval(sess: &Arc<Session>, id: String, decision: ReviewDecision) {
        match decision {
            ReviewDecision::Abort => {
                sess.interrupt_task().await;
            }
            other => sess.notify_approval(&id, other).await,
        }
    }

    pub async fn add_to_history(sess: &Arc<Session>, config: &Arc<Config>, text: String) {
        let id = sess.conversation_id;
        let config = Arc::clone(config);
        tokio::spawn(async move {
            if let Err(e) = crate::message_history::append_entry(&text, &id, &config).await {
                warn!("failed to append to message history: {e}");
            }
        });
    }

    pub async fn get_history_entry_request(
        sess: &Arc<Session>,
        config: &Arc<Config>,
        sub_id: String,
        offset: usize,
        log_id: u64,
    ) {
        let config = Arc::clone(config);
        let sess_clone = Arc::clone(sess);

        tokio::spawn(async move {
            // Run lookup in blocking thread because it does file IO + locking.
            let entry_opt = tokio::task::spawn_blocking(move || {
                crate::message_history::lookup(log_id, offset, &config)
            })
            .await
            .unwrap_or(None);

            let event = Event {
                id: sub_id,
                msg: EventMsg::GetHistoryEntryResponse(
                    crate::protocol::GetHistoryEntryResponseEvent {
                        offset,
                        log_id,
                        entry: entry_opt.map(|e| codex_protocol::message_history::HistoryEntry {
                            conversation_id: e.session_id,
                            ts: e.ts,
                            text: e.text,
                        }),
                    },
                ),
            };

            sess_clone.send_event_raw(event).await;
        });
    }

    pub async fn list_mcp_tools(sess: &Session, config: &Arc<Config>, sub_id: String) {
        let mcp_connection_manager = sess.services.mcp_connection_manager.read().await;
        let snapshot = collect_mcp_snapshot_from_manager(
            &mcp_connection_manager,
            compute_auth_statuses(
                config.mcp_servers.iter(),
                config.mcp_oauth_credentials_store_mode,
            )
            .await,
        )
        .await;
        let event = Event {
            id: sub_id,
            msg: EventMsg::McpListToolsResponse(snapshot),
        };
        sess.send_event_raw(event).await;
    }

    pub async fn list_custom_prompts(sess: &Session, sub_id: String) {
        let custom_prompts: Vec<CustomPrompt> =
            if let Some(dir) = crate::custom_prompts::default_prompts_dir() {
                crate::custom_prompts::discover_prompts_in(&dir).await
            } else {
                Vec::new()
            };

        let event = Event {
            id: sub_id,
            msg: EventMsg::ListCustomPromptsResponse(ListCustomPromptsResponseEvent {
                custom_prompts,
            }),
        };
        sess.send_event_raw(event).await;
    }

    pub async fn list_skills(
        sess: &Session,
        sub_id: String,
        cwds: Vec<PathBuf>,
        force_reload: bool,
    ) {
        let cwds = if cwds.is_empty() {
            let state = sess.state.lock().await;
            vec![state.session_configuration.cwd.clone()]
        } else {
            cwds
        };
        let skills = if sess.enabled(Feature::Skills) {
            let skills_manager = &sess.services.skills_manager;
            cwds.into_iter()
                .map(|cwd| {
                    let outcome = skills_manager.skills_for_cwd_with_options(&cwd, force_reload);
                    let errors = super::errors_to_info(&outcome.errors);
                    let skills = super::skills_to_info(&outcome.skills);
                    SkillsListEntry {
                        cwd,
                        skills,
                        errors,
                    }
                })
                .collect()
        } else {
            cwds.into_iter()
                .map(|cwd| SkillsListEntry {
                    cwd,
                    skills: Vec::new(),
                    errors: Vec::new(),
                })
                .collect()
        };
        let event = Event {
            id: sub_id,
            msg: EventMsg::ListSkillsResponse(ListSkillsResponseEvent { skills }),
        };
        sess.send_event_raw(event).await;
    }

    pub async fn undo(sess: &Arc<Session>, sub_id: String) {
        let turn_context = sess.new_default_turn_with_sub_id(sub_id).await;
        sess.spawn_task(turn_context, Vec::new(), UndoTask::new())
            .await;
    }

    pub async fn compact(sess: &Arc<Session>, sub_id: String) {
        let turn_context = sess.new_default_turn_with_sub_id(sub_id).await;

        sess.spawn_task(
            Arc::clone(&turn_context),
            vec![UserInput::Text {
                text: turn_context.compact_prompt().to_string(),
            }],
            CompactTask,
        )
        .await;
    }

    pub async fn shutdown(sess: &Arc<Session>, sub_id: String) -> bool {
        sess.abort_all_tasks(TurnAbortReason::Interrupted).await;
        sess.services
            .unified_exec_manager
            .terminate_all_sessions()
            .await;
        info!("Shutting down Codex instance");

        // Gracefully flush and shutdown rollout recorder on session end so tests
        // that inspect the rollout file do not race with the background writer.
        let recorder_opt = {
            let mut guard = sess.services.rollout.lock().await;
            guard.take()
        };
        if let Some(rec) = recorder_opt
            && let Err(e) = rec.shutdown().await
        {
            warn!("failed to shutdown rollout recorder: {e}");
            let event = Event {
                id: sub_id.clone(),
                msg: EventMsg::Error(ErrorEvent {
                    message: "Failed to shutdown rollout recorder".to_string(),
                    codex_error_info: Some(CodexErrorInfo::Other),
                }),
            };
            sess.send_event_raw(event).await;
        }

        let event = Event {
            id: sub_id,
            msg: EventMsg::ShutdownComplete,
        };
        sess.send_event_raw(event).await;
        true
    }

    pub async fn review(
        sess: &Arc<Session>,
        config: &Arc<Config>,
        sub_id: String,
        review_request: ReviewRequest,
    ) {
        let turn_context = sess.new_default_turn_with_sub_id(sub_id.clone()).await;
        match resolve_review_request(review_request, config.cwd.as_path()) {
            Ok(resolved) => {
                spawn_review_thread(
                    Arc::clone(sess),
                    Arc::clone(config),
                    turn_context.clone(),
                    sub_id,
                    resolved,
                )
                .await;
            }
            Err(err) => {
                let event = Event {
                    id: sub_id,
                    msg: EventMsg::Error(ErrorEvent {
                        message: err.to_string(),
                        codex_error_info: Some(CodexErrorInfo::Other),
                    }),
                };
                sess.send_event(&turn_context, event.msg).await;
            }
        }
    }
}

/// Spawn a review thread using the given prompt.
async fn spawn_review_thread(
    sess: Arc<Session>,
    config: Arc<Config>,
    parent_turn_context: Arc<TurnContext>,
    sub_id: String,
    resolved: crate::review_prompts::ResolvedReviewRequest,
) {
    let model = config.review_model.clone();
    let review_model_family = sess
        .services
        .models_manager
        .construct_model_family(&model, &config)
        .await;
    // For reviews, disable web_search and view_image regardless of global settings.
    let mut review_features = sess.features.clone();
    review_features
        .disable(crate::features::Feature::WebSearchRequest)
        .disable(crate::features::Feature::ViewImageTool);
    let tools_config = ToolsConfig::new(&ToolsConfigParams {
        model_family: &review_model_family,
        features: &review_features,
    });

    let base_instructions = REVIEW_PROMPT.to_string();
    let review_prompt = resolved.prompt.clone();
    let provider = parent_turn_context.client.get_provider();
    let auth_manager = parent_turn_context.client.get_auth_manager();
    let model_family = review_model_family.clone();

    // Build per‑turn client with the requested model/family.
    let mut per_turn_config = (*config).clone();
    per_turn_config.model_reasoning_effort = Some(ReasoningEffortConfig::Low);
    per_turn_config.model_reasoning_summary = ReasoningSummaryConfig::Detailed;
    per_turn_config.features = review_features.clone();

    let otel_manager = parent_turn_context.client.get_otel_manager().with_model(
        config.review_model.as_str(),
        review_model_family.slug.as_str(),
    );

    let per_turn_config = Arc::new(per_turn_config);
    let client = ModelClient::new(
        per_turn_config.clone(),
        auth_manager,
        model_family.clone(),
        otel_manager,
        provider,
        per_turn_config.model_reasoning_effort,
        per_turn_config.model_reasoning_summary,
        sess.conversation_id,
        parent_turn_context.client.get_session_source(),
    );

    let review_turn_context = TurnContext {
        sub_id: sub_id.to_string(),
        client,
        tools_config,
        ghost_snapshot: parent_turn_context.ghost_snapshot.clone(),
        developer_instructions: None,
        user_instructions: None,
        base_instructions: Some(base_instructions.clone()),
        compact_prompt: parent_turn_context.compact_prompt.clone(),
        approval_policy: parent_turn_context.approval_policy,
        sandbox_policy: parent_turn_context.sandbox_policy.clone(),
        shell_environment_policy: parent_turn_context.shell_environment_policy.clone(),
        cwd: parent_turn_context.cwd.clone(),
        final_output_json_schema: None,
        codex_linux_sandbox_exe: parent_turn_context.codex_linux_sandbox_exe.clone(),
        tool_call_gate: Arc::new(ReadinessFlag::new()),
        truncation_policy: TruncationPolicy::new(&per_turn_config, model_family.truncation_policy),
    };

    // Seed the child task with the review prompt as the initial user message.
    let input: Vec<UserInput> = vec![UserInput::Text {
        text: review_prompt,
    }];
    let tc = Arc::new(review_turn_context);
    sess.spawn_task(tc.clone(), input, ReviewTask::new()).await;

    // Announce entering review mode so UIs can switch modes.
    let review_request = ReviewRequest {
        target: resolved.target,
        user_facing_hint: Some(resolved.user_facing_hint),
    };
    sess.send_event(&tc, EventMsg::EnteredReviewMode(review_request))
        .await;
}

fn skills_to_info(skills: &[SkillMetadata]) -> Vec<ProtocolSkillMetadata> {
    skills
        .iter()
        .map(|skill| ProtocolSkillMetadata {
            name: skill.name.clone(),
            description: skill.description.clone(),
            short_description: skill.short_description.clone(),
            path: skill.path.clone(),
            scope: skill.scope,
        })
        .collect()
}

fn errors_to_info(errors: &[SkillError]) -> Vec<SkillErrorInfo> {
    errors
        .iter()
        .map(|err| SkillErrorInfo {
            path: err.path.clone(),
            message: err.message.clone(),
        })
        .collect()
}

/// 驱动一次“用户提交”(UserTurn/UserInput) 的完整处理流程。
///
/// 从用户视角：你问一次问题，最后得到一次回答。
/// 从实现视角：这一个 task 里可能会跑多个 model turn：
/// - 如果模型在输出里发起工具调用（function/custom/local_shell/MCP 等），我们先执行工具，
///   把工具输出写回对话历史，然后再发起下一轮模型调用；
/// - 如果模型只输出 assistant 消息且没有工具调用，则认为本次 task 完成，并返回最后一条 assistant 文本。
pub(crate) async fn run_task(
    sess: Arc<Session>,
    turn_context: Arc<TurnContext>,
    input: Vec<UserInput>,
    cancellation_token: CancellationToken,
) -> Option<String> {
    // 防御：没有用户输入就不启动 task。
    if input.is_empty() {
        return None;
    }

    // 自动压缩：如果累计 token 已经逼近阈值，先做一次 compaction，避免后续 turn 直接撞上下文上限。
    let auto_compact_limit = turn_context
        .client
        .get_model_family()
        .auto_compact_token_limit()
        .unwrap_or(i64::MAX);
    let total_usage_tokens = sess.get_total_token_usage().await;
    if total_usage_tokens >= auto_compact_limit {
        run_auto_compact(&sess, &turn_context).await;
    }

    // 通知前端：开始处理本次用户提交（同时带上模型可用上下文窗口信息）。
    // 这里构造一个“任务已开始”的事件消息；前端（TUI/CLI 等）订阅事件流后会据此更新状态（例如显示“正在处理”）。
    let event = EventMsg::TaskStarted(TaskStartedEvent {
        // 把本次 turn 所用模型的上下文窗口大小带给前端，用于展示/计算可用 token 上限等 UI 信息。
        model_context_window: turn_context.client.get_model_context_window(),
    });
    // 通过 Session::send_event 把事件发到内部事件通道（同时会持久化到 rollout），前端从 Codex 的 rx_event 消费到该事件。
    sess.send_event(&turn_context, event).await;

    // 如果启用了 Skills 特性，则按当前 cwd 加载技能；否则为 None。
    // then(|| ...) 只在 enabled(...) 为 true 时执行闭包，避免不必要的 IO/计算。
    let skills_outcome = sess.enabled(Feature::Skills).then(|| {
        // 通过 skills_manager 基于 cwd 解析可用技能集合（例如读取 .codex/skills 或项目内 skill 定义）。
        sess.services
            .skills_manager
            .skills_for_cwd(&turn_context.cwd)
    });

    // Skills：按当前 cwd 加载可用技能，把技能的“注入消息/提示”写入对话历史，让模型本 turn 就能看到。
    let SkillInjections {
        items: skill_items,
        warnings: skill_warnings,
    } = build_skill_injections(&input, skills_outcome.as_ref()).await;

    for message in skill_warnings {
        sess.send_event(&turn_context, EventMsg::Warning(WarningEvent { message }))
            .await;
    }

    let initial_input_for_turn: ResponseInputItem = ResponseInputItem::from(input);
    let response_item: ResponseItem = initial_input_for_turn.clone().into();
    // 把用户输入写进对话历史
    sess.record_response_item_and_emit_turn_item(turn_context.as_ref(), response_item)
        .await;

    if !skill_items.is_empty() {
        sess.record_conversation_items(&turn_context, &skill_items)
            .await;
    }

    sess.maybe_start_ghost_snapshot(Arc::clone(&turn_context), cancellation_token.child_token())
        .await;
    let mut last_agent_message: Option<String> = None;
    // TurnDiffTracker 的生命周期对实现来说覆盖整个 task（可能包含多轮 turn）；
    // 但对用户来说，它对应的是“这一次提问”整体产生的变更 diff。
    let turn_diff_tracker = Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new()));

    loop {
        // pending_input：用户可能在模型还在跑的时候又在 UI 里输入了内容；这些输入会被暂存到 pending。
        // 这里把它们也并入历史，保证下一次发给模型的 prompt 是“最新的全量上下文”。
        let pending_input = sess
            .get_pending_input()
            .await
            .into_iter()
            .map(ResponseItem::from)
            .collect::<Vec<ResponseItem>>();

        // 组装发给模型的输入：先把 pending_input 追加进历史，再取“截止当前”的完整历史作为本轮 prompt。
        let turn_input: Vec<ResponseItem> = {
            sess.record_conversation_items(&turn_context, &pending_input)
                .await;
            sess.clone_history().await.get_history_for_prompt()
        };

        let turn_input_messages = turn_input
            .iter()
            .filter_map(|item| match parse_turn_item(item) {
                Some(TurnItem::UserMessage(user_message)) => Some(user_message),
                _ => None,
            })
            .map(|user_message| user_message.message())
            .collect::<Vec<String>>();

        // 跑一轮模型（流式）：内部会解析输出 item；遇到 tool call 会执行工具并把输出写回历史。
        // 返回 needs_follow_up=true 通常表示这轮触发了工具调用，需要带着工具输出再问模型一次。
        match run_turn(
            Arc::clone(&sess),
            Arc::clone(&turn_context),
            Arc::clone(&turn_diff_tracker),
            turn_input,
            cancellation_token.child_token(),
        )
        .await
        {
            // run_turn结果处理
            Ok(turn_output) => {
                let TurnRunResult {
                    needs_follow_up,
                    last_agent_message: turn_last_agent_message,
                } = turn_output;
                let total_usage_tokens = sess.get_total_token_usage().await;
                let token_limit_reached = total_usage_tokens >= auto_compact_limit;

                // 触发了工具调用且 token 已到阈值：先 compaction 再继续 follow-up，避免上下文溢出导致失败或反复重试。
                if token_limit_reached && needs_follow_up {
                    run_auto_compact(&sess, &turn_context).await;
                    continue;
                }

                if !needs_follow_up {
                    // 没有后续：说明这一轮没有工具调用，模型已经给出最终回答，本次 task 就可以结束了。
                    last_agent_message = turn_last_agent_message;
                    sess.notifier()
                        .notify(&UserNotification::AgentTurnComplete {
                            thread_id: sess.conversation_id.to_string(),
                            turn_id: turn_context.sub_id.clone(),
                            cwd: turn_context.cwd.display().to_string(),
                            input_messages: turn_input_messages,
                            last_assistant_message: last_agent_message.clone(),
                        });
                    break;
                }
                continue;
            }
            Err(CodexErr::TurnAborted) => {
                // Turn 被中断：中断事件由其他路径上报，这里直接结束本次 task。
                break;
            }
            Err(CodexErr::InvalidImageRequest()) => {
                let mut state = sess.state.lock().await;
                error_or_panic(
                    "Invalid image detected, replacing it in the last turn to prevent poisoning",
                );
                state.history.replace_last_turn_images("Invalid image");
            }
            Err(e) => {
                info!("Turn error: {e:#}");
                let event = EventMsg::Error(e.to_error_event(None));
                sess.send_event(&turn_context, event).await;
                // 其它错误：发 ErrorEvent 给前端/用户，但不中断整个会话（用户还能继续对话）。
                break;
            }
        }
    }

    last_agent_message
}

async fn run_auto_compact(sess: &Arc<Session>, turn_context: &Arc<TurnContext>) {
    if should_use_remote_compact_task(sess.as_ref(), &turn_context.client.get_provider()) {
        run_inline_remote_auto_compact_task(Arc::clone(sess), Arc::clone(turn_context)).await;
    } else {
        run_inline_auto_compact_task(Arc::clone(sess), Arc::clone(turn_context)).await;
    }
}

/// 执行一次模型 turn（一次“向模型发起请求 → 接收流式响应”的完整过程）。
///
/// 这一层主要做两件事：
/// 1) 组装本轮 prompt：输入历史 + 工具列表 + 指令/输出 schema 覆盖；
/// 2) 负责 turn 级别的“可重试错误”处理（尤其是流式断连/超时等），并把重试信息通知给前端。
///
/// 另外该函数带有 tracing `#[instrument]` 打点：会在 trace 级别记录 turn_id/model/cwd，便于排查问题。
#[instrument(level = "trace",
    skip_all,
    fields(
        turn_id = %turn_context.sub_id,
        model = %turn_context.client.get_model(),
        cwd = %turn_context.cwd.display()
    )
)]
async fn run_turn(
    sess: Arc<Session>,
    turn_context: Arc<TurnContext>,
    turn_diff_tracker: SharedTurnDiffTracker,
    input: Vec<ResponseItem>,
    cancellation_token: CancellationToken,
) -> CodexResult<TurnRunResult> {
    // 取出所有 MCP server 当前暴露的工具。
    // 这些工具可能随用户启停 server 而变化，所以每个 turn 都重新拉取一遍，保证模型看到的是“当前可用工具集合”。
    let mcp_tools = sess
        .services
        .mcp_connection_manager
        .read()
        .await
        .list_all_tools()
        .or_cancel(&cancellation_token)
        .await?;

    // ToolRouter 是“工具的目录 + 路由器”：
    // - specs(): 把工具声明（JSON Schema 等）提供给模型，让模型知道有哪些工具可用、参数长什么样；
    // - dispatch_tool_call(): 当模型发起工具调用时，负责把调用分发到正确的 handler/runtime，产出 ResponseInputItem（随后会被写回对话历史）。
    // from_config(): 构建工具注册器：根据配置/特性/MCP 工具生成“工具声明 + 处理器”集合，供模型暴露可用工具并在调用时路由到对应 handler。
    let router = Arc::new(ToolRouter::from_config(
        &turn_context.tools_config,
        Some(
            mcp_tools
                .into_iter()
                .map(|(name, tool)| (name, tool.tool))
                .collect(),
        ),
    ));

    // 模型是否“能力上”支持 parallel tool calls（并行工具调用）。
    // 注意：最终是否启用还要受 feature 开关控制（见下方 parallel_tool_calls 字段）。
    let model_supports_parallel = turn_context
        .client
        .get_model_family()
        .supports_parallel_tool_calls;

    // 构造本轮 Prompt（也就是最终发给模型的 payload 核心部分）。
    let prompt = Prompt {
        // 本轮输入（一般是“到目前为止的完整对话历史”，由上层 run_task 组装传入）。
        input,
        // 让模型“看见”哪些工具可用（包含内置工具 + MCP 工具）。
        tools: router.specs(),
        // 是否允许模型在同一轮里并行发起多个工具调用：
        // - 需要模型本身支持
        // - 需要本次会话开启 ParallelToolCalls feature
        parallel_tool_calls: model_supports_parallel && sess.enabled(Feature::ParallelToolCalls),
        // 允许用 TurnContext 覆盖模型家族默认的 base instructions（即系统提示词的主体）。
        // 如果为 None，则会回退到 ModelFamily.base_instructions。
        base_instructions_override: turn_context.base_instructions.clone(),
        // 可选的输出 schema（主要用于 Responses API 的 structured output/JSON schema 等能力）。
        // 如果底层 provider 走的是 Chat Completions API，这个字段会被更下层拒绝（见 client.rs 里的保护）。
        output_schema: turn_context.final_output_json_schema.clone(),
    };

    // turn 级别重试计数：只针对“可重试”的流式错误。
    let mut retries = 0;
    loop {
        // try_run_turn 负责真正的流式收包、解析 output item、触发工具调用、把所有产物写回历史等。
        // ToolRouter和router.specs()有啥区别？
        // router.specs()是工具清单
        // ToolRouter除了工具清单，还有registry.dispatch()（执行器）
        match try_run_turn(
            Arc::clone(&router),
            Arc::clone(&sess),
            Arc::clone(&turn_context),
            Arc::clone(&turn_diff_tracker),
            &prompt,
            cancellation_token.child_token(),
        )
        .await
        {
            // 成功完成一轮 turn：返回是否需要 follow-up（通常由工具调用驱动）以及本轮最后一条 assistant 文本。
            Ok(output) => return Ok(output),

            // 这些错误都属于“明确不该在这里重试”的类型：要么是用户取消/中断，要么是配置/请求本身有问题，
            // 或者是配额/上下文上限等逻辑性失败，继续重试也不会变好。
            Err(CodexErr::TurnAborted) => {
                return Err(CodexErr::TurnAborted);
            }
            Err(CodexErr::Interrupted) => return Err(CodexErr::Interrupted),
            Err(CodexErr::EnvVar(var)) => return Err(CodexErr::EnvVar(var)),
            Err(e @ CodexErr::Fatal(_)) => return Err(e),
            Err(e @ CodexErr::ContextWindowExceeded) => {
                // 上下文窗口溢出：标记内部 token 状态“已满”，便于 UI/逻辑做出提示或触发压缩。
                sess.set_total_tokens_full(&turn_context).await;
                return Err(e);
            }
            Err(CodexErr::UsageLimitReached(e)) => {
                // 触发速率限制：把服务端返回的 rate_limits 写入 session 状态，方便 UI 展示/后续逻辑决策。
                let rate_limits = e.rate_limits.clone();
                if let Some(rate_limits) = rate_limits {
                    sess.update_rate_limits(&turn_context, rate_limits).await;
                }
                return Err(CodexErr::UsageLimitReached(e));
            }
            Err(CodexErr::UsageNotIncluded) => return Err(CodexErr::UsageNotIncluded),
            Err(e @ CodexErr::QuotaExceeded) => return Err(e),
            Err(e @ CodexErr::InvalidImageRequest()) => return Err(e),
            Err(e @ CodexErr::InvalidRequest(_)) => return Err(e),
            Err(e @ CodexErr::RefreshTokenFailed(_)) => return Err(e),
            Err(e) => {
                // 兜底：把其它错误视为“可能是流式断连/临时性网络问题”，走有限次重试。
                // 重试上限由 provider 决定（不同 provider/环境对 SSE 断连容忍度不同）。
                let max_retries = turn_context.client.get_provider().stream_max_retries();
                if retries < max_retries {
                    retries += 1;
                    let delay = match e {
                        // 某些流式错误会携带建议的重试等待时间，优先遵从它。
                        CodexErr::Stream(_, Some(delay)) => delay,
                        // 否则使用带抖动的指数退避（见 util::backoff），避免立刻重连把错误放大。
                        _ => backoff(retries),
                    };
                    warn!(
                        "stream disconnected - retrying turn ({retries}/{max_retries} in {delay:?})...",
                    );

                    // 把“正在重连”的信息显式发给前端：避免用户以为卡死。
                    // 这里还会把错误细节编码进事件里（用于日志/诊断）。
                    sess.notify_stream_error(
                        &turn_context,
                        format!("Reconnecting... {retries}/{max_retries}"),
                        e,
                    )
                    .await;

                    tokio::time::sleep(delay).await;
                } else {
                    // 超过重试预算：把原错误上抛，由上层（run_task）负责发 ErrorEvent/结束本次 turn。
                    return Err(e);
                }
            }
        }
    }
}

#[derive(Debug)]
struct TurnRunResult {
    needs_follow_up: bool,
    last_agent_message: Option<String>,
}

async fn drain_in_flight(
    in_flight: &mut FuturesOrdered<BoxFuture<'static, CodexResult<ResponseInputItem>>>,
    sess: Arc<Session>,
    turn_context: Arc<TurnContext>,
) -> CodexResult<()> {
    while let Some(res) = in_flight.next().await {
        match res {
            Ok(response_input) => {
                sess.record_conversation_items(&turn_context, &[response_input.into()])
                    .await;
            }
            Err(err) => {
                error_or_panic(format!("in-flight tool future failed during drain: {err}"));
            }
        }
    }
    Ok(())
}

/// 实际执行一次 turn 的“流式事件循环”：
/// - 先把本轮 `TurnContext` 作为 `RolloutItem::TurnContext` 持久化（用于回放/诊断/恢复）；
/// - 再发起流式请求并持续消费 `ResponseEvent`；
/// - 在 `OutputItemDone` 时落盘完整 output item，并在需要时启动工具调用；
/// - 在 `Completed` 时写入 token usage，并在 turn 结束后统一等待所有工具调用收敛，然后（可选）发出 turn diff。
///
/// 注意：`run_turn` 负责 prompt 组装和“可重试错误”的重试；这里专注于一次请求的完整生命周期。
#[allow(clippy::too_many_arguments)]
#[instrument(level = "trace",
    skip_all,
    fields(
        turn_id = %turn_context.sub_id,
        model = %turn_context.client.get_model()
    )
)]
async fn try_run_turn(
    router: Arc<ToolRouter>,
    sess: Arc<Session>,
    turn_context: Arc<TurnContext>,
    turn_diff_tracker: SharedTurnDiffTracker,
    prompt: &Prompt,
    cancellation_token: CancellationToken,
) -> CodexResult<TurnRunResult> {
    // 把 TurnContext 的关键字段写入 rollout（独立于 ResponseItem），用于：
    // - 离线回放：复原当时的 cwd/模型/指令/审批与沙盒策略/输出 schema/截断策略等；
    // - 排障：即使 stream 中途断开，也能准确知道“这轮请求是在什么上下文里发出的”。
    let rollout_item = RolloutItem::TurnContext(TurnContextItem {
        cwd: turn_context.cwd.clone(),
        approval_policy: turn_context.approval_policy,
        sandbox_policy: turn_context.sandbox_policy.clone(),
        model: turn_context.client.get_model(),
        effort: turn_context.client.get_reasoning_effort(),
        summary: turn_context.client.get_reasoning_summary(),
        base_instructions: turn_context.base_instructions.clone(),
        user_instructions: turn_context.user_instructions.clone(),
        developer_instructions: turn_context.developer_instructions.clone(),
        final_output_json_schema: turn_context.final_output_json_schema.clone(),
        truncation_policy: Some(turn_context.truncation_policy.into()),
    });

    // 先持久化上下文再开始收包：确保 rollout 的记录顺序与实际执行顺序一致。
    sess.persist_rollout_items(&[rollout_item]).await;

    // 发起一次 provider 的流式请求（SSE/stream）。
    // `or_cancel(...).await??`：
    // - 外层 `?` 把 `CancelErr::Cancelled` 转为 `CodexErr::TurnAborted`；
    // - 内层 `?` 传播 provider/client 侧的业务错误（`CodexErr`）。
    let mut stream = turn_context
        .client
        .clone()
        // stream 方法很关键，发送真正的请求，然后将流式输出转为ResponseEvent
        .stream(prompt)
        .instrument(trace_span!("stream_request"))
        .or_cancel(&cancellation_token)
        .await??;

    // 负责把“模型产生的工具调用”路由到具体 handler，并把工具输出包装成 `ResponseInputItem` 写回对话历史。
    // 内部会结合 `turn_diff_tracker` 跟踪 apply_patch 造成的文件变更（用于 turn diff）。
    let tool_runtime = ToolCallRuntime::new(
        Arc::clone(&router),
        Arc::clone(&sess),
        Arc::clone(&turn_context),
        Arc::clone(&turn_diff_tracker),
    );

    // 运行中的工具 future（按“工具调用出现的顺序”保存并在 turn 尾部依次 drain）。
    // 之所以用 `FuturesOrdered`：
    // - 工具执行在 `ToolCallRuntime::handle_tool_call` 内部会 `tokio::spawn`，可以与收包并行；
    // - 但工具输出写回历史的顺序希望稳定（与模型发出调用的顺序一致），因此这里按序等待并记录。
    let mut in_flight: FuturesOrdered<BoxFuture<'static, CodexResult<ResponseInputItem>>> =
        FuturesOrdered::new();

    // 汇总给上层 run_turn 的 turn 级信息：
    // - needs_follow_up：是否产生了工具调用/工具输出等，需要下一轮继续；
    // - last_agent_message：本轮最后一条 assistant 文本（若有）。
    let mut needs_follow_up = false;
    let mut last_agent_message: Option<String> = None;

    // 当前“正在流式输出”的 TurnItem（用于给 *Delta 事件提供 item_id）。
    // 在 `OutputItemAdded` 时设置，在 `OutputItemDone` 时清空。
    let mut active_item: Option<TurnItem> = None;

    // 只有当收到了 `ResponseEvent::Completed` 才会置为 true；
    // turn diff 会在所有工具调用都 drain 完后再计算/发送。
    let mut should_emit_turn_diff = false;
    let receiving_span = trace_span!("receiving_stream");
    let outcome: CodexResult<TurnRunResult> = loop {
        // 每处理一个事件都创建一个子 span，便于把 OTel/trace 字段（tool_name 等）挂在正确的事件上。
        let handle_responses = trace_span!(
            parent: &receiving_span,
            "handle_responses",
            otel.name = field::Empty,
            tool_name = field::Empty,
            from = field::Empty,
        );

        // 从 stream 中取下一个事件；如果 turn 被取消则直接中止整轮请求。
        let event = match stream
            .next()
            .instrument(trace_span!(parent: &handle_responses, "receiving"))
            .or_cancel(&cancellation_token)
            .await
        {
            Ok(event) => event,
            Err(codex_async_utils::CancelErr::Cancelled) => break Err(CodexErr::TurnAborted),
        };

        // provider 的 stream 正常情况下应该以 `ResponseEvent::Completed` 收尾；
        // 如果提前结束（None），视为流式连接异常断开。
        let event = match event {
            Some(res) => res?,
            None => {
                break Err(CodexErr::Stream(
                    "stream closed before response.completed".into(),
                    None,
                ));
            }
        };

        // 把 stream 原始事件记录到可观测性管道里（便于诊断/追踪一次 turn 的完整事件序列）。
        sess.services
            .otel_manager
            .record_responses(&handle_responses, &event);

        match event {
            ResponseEvent::Created => {}
            ResponseEvent::OutputItemDone(item) => {
                // 一个 output item 已经“完整结束”，后续不会再收到它的 *Delta。
                // 这里把 `active_item` take 掉：
                // - 防止后续 delta 误关联到已结束的 item；
                // - 并把 previously_active_item 传给 `handle_output_item_done`，
                //   让它决定是否需要补发 `emit_turn_item_started`（有些 provider 可能不会发 OutputItemAdded）。
                let previously_active_item = active_item.take();
                let mut ctx = HandleOutputCtx {
                    sess: sess.clone(),
                    turn_context: turn_context.clone(),
                    tool_runtime: tool_runtime.clone(),
                    cancellation_token: cancellation_token.child_token(),
                };

                // 统一处理“完整 item”：
                // - 若是普通消息/推理/websearch：转换为 TurnItem，发 started/completed 事件，并写入对话历史；
                // - 若是工具调用：先写入对话历史，再启动工具执行（返回一个 future，稍后会被 drain 写回工具输出）；
                // - 若是工具调用的 guardrail/拒绝等：也会把需要回写给模型的输出写入历史，并标记 needs_follow_up。
                let output_result = handle_output_item_done(&mut ctx, item, previously_active_item)
                    .instrument(handle_responses)
                    .await?;
                if let Some(tool_future) = output_result.tool_future {
                    in_flight.push_back(tool_future);
                }
                if let Some(agent_message) = output_result.last_agent_message {
                    last_agent_message = Some(agent_message);
                }
                needs_follow_up |= output_result.needs_follow_up;
            }
            ResponseEvent::OutputItemAdded(item) => {
                // 一个新的 output item 开始出现：把它转换成 TurnItem 并立刻发 started，
                // 这样后续的 `OutputTextDelta/Reasoning*Delta` 才能携带稳定的 item_id 做流式渲染。
                if let Some(turn_item) = handle_non_tool_response_item(&item).await {
                    let tracked_item = turn_item.clone();
                    sess.emit_turn_item_started(&turn_context, &turn_item).await;

                    active_item = Some(tracked_item);
                }
            }
            ResponseEvent::RateLimits(snapshot) => {
                // 更新内部的 rate limits 快照，但延后到拿到 token usage 后再对外发事件：
                // 避免出现重复的 TokenCount（rate limits 通常会随着 usage 一起展示）。
                sess.update_rate_limits(&turn_context, snapshot).await;
            }
            ResponseEvent::ModelsEtag(etag) => {
                // 更新 models 列表的 etag：如果有新版本则触发刷新，让 UI/逻辑能尽快看到模型变化。
                sess.services.models_manager.refresh_if_new_etag(etag).await;
            }
            ResponseEvent::Completed {
                response_id: _,
                token_usage,
            } => {
                // `Completed` 是一轮 stream 的正常结束标志：
                // - token usage 在这里才完整可用；
                // - 同时也意味着不会再有新的 output item/delta。
                sess.update_token_usage_info(&turn_context, token_usage.as_ref())
                    .await;
                should_emit_turn_diff = true;

                break Ok(TurnRunResult {
                    needs_follow_up,
                    last_agent_message,
                });
            }
            ResponseEvent::OutputTextDelta(delta) => {
                // assistant 文本的增量输出：需要关联到当前 active item。
                // 如果没有 active item，说明 stream 事件顺序不符合预期：debug 下直接 panic 便于尽早发现。
                if let Some(active) = active_item.as_ref() {
                    let event = AgentMessageContentDeltaEvent {
                        thread_id: sess.conversation_id.to_string(),
                        turn_id: turn_context.sub_id.clone(),
                        item_id: active.id(),
                        delta: delta.clone(),
                    };
                    sess.send_event(&turn_context, EventMsg::AgentMessageContentDelta(event))
                        .await;
                } else {
                    error_or_panic("OutputTextDelta without active item".to_string());
                }
            }
            ResponseEvent::ReasoningSummaryDelta {
                delta,
                summary_index,
            } => {
                // 推理摘要的增量输出（summary 可能分段/多段，所以会带 index）。
                if let Some(active) = active_item.as_ref() {
                    let event = ReasoningContentDeltaEvent {
                        thread_id: sess.conversation_id.to_string(),
                        turn_id: turn_context.sub_id.clone(),
                        item_id: active.id(),
                        delta,
                        summary_index,
                    };
                    sess.send_event(&turn_context, EventMsg::ReasoningContentDelta(event))
                        .await;
                } else {
                    error_or_panic("ReasoningSummaryDelta without active item".to_string());
                }
            }
            ResponseEvent::ReasoningSummaryPartAdded { summary_index } => {
                // 推理摘要新增一个分段：UI 可以在该处插入“分隔线/段落分割”。
                if let Some(active) = active_item.as_ref() {
                    let event =
                        EventMsg::AgentReasoningSectionBreak(AgentReasoningSectionBreakEvent {
                            item_id: active.id(),
                            summary_index,
                        });
                    sess.send_event(&turn_context, event).await;
                } else {
                    error_or_panic("ReasoningSummaryPartAdded without active item".to_string());
                }
            }
            ResponseEvent::ReasoningContentDelta {
                delta,
                content_index,
            } => {
                // 推理 raw content 的增量输出（通常用于展示更底层的推理 token 流）。
                if let Some(active) = active_item.as_ref() {
                    let event = ReasoningRawContentDeltaEvent {
                        thread_id: sess.conversation_id.to_string(),
                        turn_id: turn_context.sub_id.clone(),
                        item_id: active.id(),
                        delta,
                        content_index,
                    };
                    sess.send_event(&turn_context, EventMsg::ReasoningRawContentDelta(event))
                        .await;
                } else {
                    error_or_panic("ReasoningRawContentDelta without active item".to_string());
                }
            }
        }
    };

    // 等待本轮里启动的所有工具调用完成，并把工具输出写回对话历史。
    // 即使 stream 因取消/错误退出，也需要 drain 一次，避免工具任务悬挂以及历史“缺一截输出”。
    drain_in_flight(&mut in_flight, sess.clone(), turn_context.clone()).await?;

    if should_emit_turn_diff {
        // 只有在 turn 正常 Completed 后才尝试发 turn diff。
        // turn diff 的计算需要看到工具执行（尤其是 apply_patch）最终落盘的文件状态，
        // 所以必须放在 drain_in_flight 之后。
        let unified_diff = {
            let mut tracker = turn_diff_tracker.lock().await;
            tracker.get_unified_diff()
        };
        if let Ok(Some(unified_diff)) = unified_diff {
            let msg = EventMsg::TurnDiff(TurnDiffEvent { unified_diff });
            sess.clone().send_event(&turn_context, msg).await;
        }
    }

    outcome
}

pub(super) fn get_last_assistant_message_from_turn(responses: &[ResponseItem]) -> Option<String> {
    responses.iter().rev().find_map(|item| {
        if let ResponseItem::Message { role, content, .. } = item {
            if role == "assistant" {
                content.iter().rev().find_map(|ci| {
                    if let ContentItem::OutputText { text } = ci {
                        Some(text.clone())
                    } else {
                        None
                    }
                })
            } else {
                None
            }
        } else {
            None
        }
    })
}

#[cfg(test)]
pub(crate) use tests::make_session_and_context;

#[cfg(test)]
pub(crate) use tests::make_session_and_context_with_rx;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CodexAuth;
    use crate::config::ConfigBuilder;
    use crate::exec::ExecToolCallOutput;
    use crate::function_tool::FunctionCallError;
    use crate::shell::default_user_shell;
    use crate::tools::format_exec_output_str;

    use codex_protocol::models::FunctionCallOutputPayload;

    use crate::protocol::CompactedItem;
    use crate::protocol::CreditsSnapshot;
    use crate::protocol::InitialHistory;
    use crate::protocol::RateLimitSnapshot;
    use crate::protocol::RateLimitWindow;
    use crate::protocol::ResumedHistory;
    use crate::state::TaskKind;
    use crate::tasks::SessionTask;
    use crate::tasks::SessionTaskContext;
    use crate::tools::ToolRouter;
    use crate::tools::context::ToolInvocation;
    use crate::tools::context::ToolOutput;
    use crate::tools::context::ToolPayload;
    use crate::tools::handlers::ShellHandler;
    use crate::tools::handlers::UnifiedExecHandler;
    use crate::tools::registry::ToolHandler;
    use crate::turn_diff_tracker::TurnDiffTracker;
    use codex_app_server_protocol::AuthMode;
    use codex_protocol::models::ContentItem;
    use codex_protocol::models::ResponseItem;
    use std::path::Path;
    use std::time::Duration;
    use tokio::time::sleep;

    use mcp_types::ContentBlock;
    use mcp_types::TextContent;
    use pretty_assertions::assert_eq;
    use serde::Deserialize;
    use serde_json::json;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::Duration as StdDuration;

    #[tokio::test]
    async fn reconstruct_history_matches_live_compactions() {
        let (session, turn_context) = make_session_and_context().await;
        let (rollout_items, expected) = sample_rollout(&session, &turn_context);

        let reconstructed = session.reconstruct_history_from_rollout(&turn_context, &rollout_items);

        assert_eq!(expected, reconstructed);
    }

    #[tokio::test]
    async fn record_initial_history_reconstructs_resumed_transcript() {
        let (session, turn_context) = make_session_and_context().await;
        let (rollout_items, expected) = sample_rollout(&session, &turn_context);

        session
            .record_initial_history(InitialHistory::Resumed(ResumedHistory {
                conversation_id: ConversationId::default(),
                history: rollout_items,
                rollout_path: PathBuf::from("/tmp/resume.jsonl"),
            }))
            .await;

        let actual = session.state.lock().await.clone_history().get_history();
        assert_eq!(expected, actual);
    }

    #[tokio::test]
    async fn record_initial_history_reconstructs_forked_transcript() {
        let (session, turn_context) = make_session_and_context().await;
        let (rollout_items, expected) = sample_rollout(&session, &turn_context);

        session
            .record_initial_history(InitialHistory::Forked(rollout_items))
            .await;

        let actual = session.state.lock().await.clone_history().get_history();
        assert_eq!(expected, actual);
    }

    #[tokio::test]
    async fn set_rate_limits_retains_previous_credits() {
        let codex_home = tempfile::tempdir().expect("create temp dir");
        let config = build_test_config(codex_home.path()).await;
        let config = Arc::new(config);
        let model = ModelsManager::get_model_offline(config.model.as_deref());
        let session_configuration = SessionConfiguration {
            provider: config.model_provider.clone(),
            model,
            model_reasoning_effort: config.model_reasoning_effort,
            model_reasoning_summary: config.model_reasoning_summary,
            developer_instructions: config.developer_instructions.clone(),
            user_instructions: config.user_instructions.clone(),
            base_instructions: config.base_instructions.clone(),
            compact_prompt: config.compact_prompt.clone(),
            approval_policy: config.approval_policy.clone(),
            sandbox_policy: config.sandbox_policy.clone(),
            cwd: config.cwd.clone(),
            original_config_do_not_use: Arc::clone(&config),
            session_source: SessionSource::Exec,
        };

        let mut state = SessionState::new(session_configuration);
        let initial = RateLimitSnapshot {
            primary: Some(RateLimitWindow {
                used_percent: 10.0,
                window_minutes: Some(15),
                resets_at: Some(1_700),
            }),
            secondary: None,
            credits: Some(CreditsSnapshot {
                has_credits: true,
                unlimited: false,
                balance: Some("10.00".to_string()),
            }),
            plan_type: Some(codex_protocol::account::PlanType::Plus),
        };
        state.set_rate_limits(initial.clone());

        let update = RateLimitSnapshot {
            primary: Some(RateLimitWindow {
                used_percent: 40.0,
                window_minutes: Some(30),
                resets_at: Some(1_800),
            }),
            secondary: Some(RateLimitWindow {
                used_percent: 5.0,
                window_minutes: Some(60),
                resets_at: Some(1_900),
            }),
            credits: None,
            plan_type: None,
        };
        state.set_rate_limits(update.clone());

        assert_eq!(
            state.latest_rate_limits,
            Some(RateLimitSnapshot {
                primary: update.primary.clone(),
                secondary: update.secondary,
                credits: initial.credits,
                plan_type: initial.plan_type,
            })
        );
    }

    #[tokio::test]
    async fn set_rate_limits_updates_plan_type_when_present() {
        let codex_home = tempfile::tempdir().expect("create temp dir");
        let config = build_test_config(codex_home.path()).await;
        let config = Arc::new(config);
        let model = ModelsManager::get_model_offline(config.model.as_deref());
        let session_configuration = SessionConfiguration {
            provider: config.model_provider.clone(),
            model,
            model_reasoning_effort: config.model_reasoning_effort,
            model_reasoning_summary: config.model_reasoning_summary,
            developer_instructions: config.developer_instructions.clone(),
            user_instructions: config.user_instructions.clone(),
            base_instructions: config.base_instructions.clone(),
            compact_prompt: config.compact_prompt.clone(),
            approval_policy: config.approval_policy.clone(),
            sandbox_policy: config.sandbox_policy.clone(),
            cwd: config.cwd.clone(),
            original_config_do_not_use: Arc::clone(&config),
            session_source: SessionSource::Exec,
        };

        let mut state = SessionState::new(session_configuration);
        let initial = RateLimitSnapshot {
            primary: Some(RateLimitWindow {
                used_percent: 15.0,
                window_minutes: Some(20),
                resets_at: Some(1_600),
            }),
            secondary: Some(RateLimitWindow {
                used_percent: 5.0,
                window_minutes: Some(45),
                resets_at: Some(1_650),
            }),
            credits: Some(CreditsSnapshot {
                has_credits: true,
                unlimited: false,
                balance: Some("15.00".to_string()),
            }),
            plan_type: Some(codex_protocol::account::PlanType::Plus),
        };
        state.set_rate_limits(initial.clone());

        let update = RateLimitSnapshot {
            primary: Some(RateLimitWindow {
                used_percent: 35.0,
                window_minutes: Some(25),
                resets_at: Some(1_700),
            }),
            secondary: None,
            credits: None,
            plan_type: Some(codex_protocol::account::PlanType::Pro),
        };
        state.set_rate_limits(update.clone());

        assert_eq!(
            state.latest_rate_limits,
            Some(RateLimitSnapshot {
                primary: update.primary,
                secondary: update.secondary,
                credits: initial.credits,
                plan_type: update.plan_type,
            })
        );
    }

    #[test]
    fn prefers_structured_content_when_present() {
        let ctr = CallToolResult {
            // Content present but should be ignored because structured_content is set.
            content: vec![text_block("ignored")],
            is_error: None,
            structured_content: Some(json!({
                "ok": true,
                "value": 42
            })),
        };

        let got = FunctionCallOutputPayload::from(&ctr);
        let expected = FunctionCallOutputPayload {
            content: serde_json::to_string(&json!({
                "ok": true,
                "value": 42
            }))
            .unwrap(),
            success: Some(true),
            ..Default::default()
        };

        assert_eq!(expected, got);
    }

    #[tokio::test]
    async fn includes_timed_out_message() {
        let exec = ExecToolCallOutput {
            exit_code: 0,
            stdout: StreamOutput::new(String::new()),
            stderr: StreamOutput::new(String::new()),
            aggregated_output: StreamOutput::new("Command output".to_string()),
            duration: StdDuration::from_secs(1),
            timed_out: true,
        };
        let (_, turn_context) = make_session_and_context().await;

        let out = format_exec_output_str(&exec, turn_context.truncation_policy);

        assert_eq!(
            out,
            "command timed out after 1000 milliseconds\nCommand output"
        );
    }

    #[test]
    fn falls_back_to_content_when_structured_is_null() {
        let ctr = CallToolResult {
            content: vec![text_block("hello"), text_block("world")],
            is_error: None,
            structured_content: Some(serde_json::Value::Null),
        };

        let got = FunctionCallOutputPayload::from(&ctr);
        let expected = FunctionCallOutputPayload {
            content: serde_json::to_string(&vec![text_block("hello"), text_block("world")])
                .unwrap(),
            success: Some(true),
            ..Default::default()
        };

        assert_eq!(expected, got);
    }

    #[test]
    fn success_flag_reflects_is_error_true() {
        let ctr = CallToolResult {
            content: vec![text_block("unused")],
            is_error: Some(true),
            structured_content: Some(json!({ "message": "bad" })),
        };

        let got = FunctionCallOutputPayload::from(&ctr);
        let expected = FunctionCallOutputPayload {
            content: serde_json::to_string(&json!({ "message": "bad" })).unwrap(),
            success: Some(false),
            ..Default::default()
        };

        assert_eq!(expected, got);
    }

    #[test]
    fn success_flag_true_with_no_error_and_content_used() {
        let ctr = CallToolResult {
            content: vec![text_block("alpha")],
            is_error: Some(false),
            structured_content: None,
        };

        let got = FunctionCallOutputPayload::from(&ctr);
        let expected = FunctionCallOutputPayload {
            content: serde_json::to_string(&vec![text_block("alpha")]).unwrap(),
            success: Some(true),
            ..Default::default()
        };

        assert_eq!(expected, got);
    }

    fn text_block(s: &str) -> ContentBlock {
        ContentBlock::TextContent(TextContent {
            annotations: None,
            text: s.to_string(),
            r#type: "text".to_string(),
        })
    }

    async fn build_test_config(codex_home: &Path) -> Config {
        ConfigBuilder::default()
            .codex_home(codex_home.to_path_buf())
            .build()
            .await
            .expect("load default test config")
    }

    fn otel_manager(
        conversation_id: ConversationId,
        config: &Config,
        model_family: &ModelFamily,
        session_source: SessionSource,
    ) -> OtelManager {
        OtelManager::new(
            conversation_id,
            ModelsManager::get_model_offline(config.model.as_deref()).as_str(),
            model_family.slug.as_str(),
            None,
            Some("test@test.com".to_string()),
            Some(AuthMode::ChatGPT),
            false,
            "test".to_string(),
            session_source,
        )
    }

    pub(crate) async fn make_session_and_context() -> (Session, TurnContext) {
        let (tx_event, _rx_event) = async_channel::unbounded();
        let codex_home = tempfile::tempdir().expect("create temp dir");
        let config = build_test_config(codex_home.path()).await;
        let config = Arc::new(config);
        let conversation_id = ConversationId::default();
        let auth_manager =
            AuthManager::from_auth_for_testing(CodexAuth::from_api_key("Test API Key"));
        let models_manager = Arc::new(ModelsManager::new(auth_manager.clone()));
        let exec_policy = ExecPolicyManager::default();
        let model = ModelsManager::get_model_offline(config.model.as_deref());
        let session_configuration = SessionConfiguration {
            provider: config.model_provider.clone(),
            model,
            model_reasoning_effort: config.model_reasoning_effort,
            model_reasoning_summary: config.model_reasoning_summary,
            developer_instructions: config.developer_instructions.clone(),
            user_instructions: config.user_instructions.clone(),
            base_instructions: config.base_instructions.clone(),
            compact_prompt: config.compact_prompt.clone(),
            approval_policy: config.approval_policy.clone(),
            sandbox_policy: config.sandbox_policy.clone(),
            cwd: config.cwd.clone(),
            original_config_do_not_use: Arc::clone(&config),
            session_source: SessionSource::Exec,
        };
        let per_turn_config = Session::build_per_turn_config(&session_configuration);
        let model_family = ModelsManager::construct_model_family_offline(
            session_configuration.model.as_str(),
            &per_turn_config,
        );
        let otel_manager = otel_manager(
            conversation_id,
            config.as_ref(),
            &model_family,
            session_configuration.session_source.clone(),
        );

        let state = SessionState::new(session_configuration.clone());
        let skills_manager = Arc::new(SkillsManager::new(config.codex_home.clone()));

        let services = SessionServices {
            mcp_connection_manager: Arc::new(RwLock::new(McpConnectionManager::default())),
            mcp_startup_cancellation_token: CancellationToken::new(),
            unified_exec_manager: UnifiedExecSessionManager::default(),
            notifier: UserNotifier::new(None),
            rollout: Mutex::new(None),
            user_shell: Arc::new(default_user_shell()),
            show_raw_agent_reasoning: config.show_raw_agent_reasoning,
            exec_policy,
            auth_manager: auth_manager.clone(),
            otel_manager: otel_manager.clone(),
            models_manager: Arc::clone(&models_manager),
            tool_approvals: Mutex::new(ApprovalStore::default()),
            skills_manager,
        };

        let turn_context = Session::make_turn_context(
            Some(Arc::clone(&auth_manager)),
            &otel_manager,
            session_configuration.provider.clone(),
            &session_configuration,
            per_turn_config,
            model_family,
            conversation_id,
            "turn_id".to_string(),
        );

        let session = Session {
            conversation_id,
            tx_event,
            state: Mutex::new(state),
            features: config.features.clone(),
            active_turn: Mutex::new(None),
            services,
            next_internal_sub_id: AtomicU64::new(0),
        };

        (session, turn_context)
    }

    // Like make_session_and_context, but returns Arc<Session> and the event receiver
    // so tests can assert on emitted events.
    pub(crate) async fn make_session_and_context_with_rx() -> (
        Arc<Session>,
        Arc<TurnContext>,
        async_channel::Receiver<Event>,
    ) {
        let (tx_event, rx_event) = async_channel::unbounded();
        let codex_home = tempfile::tempdir().expect("create temp dir");
        let config = build_test_config(codex_home.path()).await;
        let config = Arc::new(config);
        let conversation_id = ConversationId::default();
        let auth_manager =
            AuthManager::from_auth_for_testing(CodexAuth::from_api_key("Test API Key"));
        let models_manager = Arc::new(ModelsManager::new(auth_manager.clone()));
        let exec_policy = ExecPolicyManager::default();
        let model = ModelsManager::get_model_offline(config.model.as_deref());
        let session_configuration = SessionConfiguration {
            provider: config.model_provider.clone(),
            model,
            model_reasoning_effort: config.model_reasoning_effort,
            model_reasoning_summary: config.model_reasoning_summary,
            developer_instructions: config.developer_instructions.clone(),
            user_instructions: config.user_instructions.clone(),
            base_instructions: config.base_instructions.clone(),
            compact_prompt: config.compact_prompt.clone(),
            approval_policy: config.approval_policy.clone(),
            sandbox_policy: config.sandbox_policy.clone(),
            cwd: config.cwd.clone(),
            original_config_do_not_use: Arc::clone(&config),
            session_source: SessionSource::Exec,
        };
        let per_turn_config = Session::build_per_turn_config(&session_configuration);
        let model_family = ModelsManager::construct_model_family_offline(
            session_configuration.model.as_str(),
            &per_turn_config,
        );
        let otel_manager = otel_manager(
            conversation_id,
            config.as_ref(),
            &model_family,
            session_configuration.session_source.clone(),
        );

        let state = SessionState::new(session_configuration.clone());
        let skills_manager = Arc::new(SkillsManager::new(config.codex_home.clone()));

        let services = SessionServices {
            mcp_connection_manager: Arc::new(RwLock::new(McpConnectionManager::default())),
            mcp_startup_cancellation_token: CancellationToken::new(),
            unified_exec_manager: UnifiedExecSessionManager::default(),
            notifier: UserNotifier::new(None),
            rollout: Mutex::new(None),
            user_shell: Arc::new(default_user_shell()),
            show_raw_agent_reasoning: config.show_raw_agent_reasoning,
            exec_policy,
            auth_manager: Arc::clone(&auth_manager),
            otel_manager: otel_manager.clone(),
            models_manager: Arc::clone(&models_manager),
            tool_approvals: Mutex::new(ApprovalStore::default()),
            skills_manager,
        };

        let turn_context = Arc::new(Session::make_turn_context(
            Some(Arc::clone(&auth_manager)),
            &otel_manager,
            session_configuration.provider.clone(),
            &session_configuration,
            per_turn_config,
            model_family,
            conversation_id,
            "turn_id".to_string(),
        ));

        let session = Arc::new(Session {
            conversation_id,
            tx_event,
            state: Mutex::new(state),
            features: config.features.clone(),
            active_turn: Mutex::new(None),
            services,
            next_internal_sub_id: AtomicU64::new(0),
        });

        (session, turn_context, rx_event)
    }

    #[tokio::test]
    async fn record_model_warning_appends_user_message() {
        let (mut session, turn_context) = make_session_and_context().await;
        let mut features = Features::with_defaults();
        features.enable(Feature::ModelWarnings);
        session.features = features;

        session
            .record_model_warning("too many unified exec sessions", &turn_context)
            .await;

        let mut history = session.clone_history().await;
        let history_items = history.get_history();
        let last = history_items.last().expect("warning recorded");

        match last {
            ResponseItem::Message { role, content, .. } => {
                assert_eq!(role, "user");
                assert_eq!(
                    content,
                    &vec![ContentItem::InputText {
                        text: "Warning: too many unified exec sessions".to_string(),
                    }]
                );
            }
            other => panic!("expected user message, got {other:?}"),
        }
    }

    #[derive(Clone, Copy)]
    struct NeverEndingTask {
        kind: TaskKind,
        listen_to_cancellation_token: bool,
    }

    #[async_trait::async_trait]
    impl SessionTask for NeverEndingTask {
        fn kind(&self) -> TaskKind {
            self.kind
        }

        async fn run(
            self: Arc<Self>,
            _session: Arc<SessionTaskContext>,
            _ctx: Arc<TurnContext>,
            _input: Vec<UserInput>,
            cancellation_token: CancellationToken,
        ) -> Option<String> {
            if self.listen_to_cancellation_token {
                cancellation_token.cancelled().await;
                return None;
            }
            loop {
                sleep(Duration::from_secs(60)).await;
            }
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[test_log::test]
    async fn abort_regular_task_emits_turn_aborted_only() {
        let (sess, tc, rx) = make_session_and_context_with_rx().await;
        let input = vec![UserInput::Text {
            text: "hello".to_string(),
        }];
        sess.spawn_task(
            Arc::clone(&tc),
            input,
            NeverEndingTask {
                kind: TaskKind::Regular,
                listen_to_cancellation_token: false,
            },
        )
        .await;

        sess.abort_all_tasks(TurnAbortReason::Interrupted).await;

        let evt = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("timeout waiting for event")
            .expect("event");
        match evt.msg {
            EventMsg::TurnAborted(e) => assert_eq!(TurnAbortReason::Interrupted, e.reason),
            other => panic!("unexpected event: {other:?}"),
        }
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn abort_gracefuly_emits_turn_aborted_only() {
        let (sess, tc, rx) = make_session_and_context_with_rx().await;
        let input = vec![UserInput::Text {
            text: "hello".to_string(),
        }];
        sess.spawn_task(
            Arc::clone(&tc),
            input,
            NeverEndingTask {
                kind: TaskKind::Regular,
                listen_to_cancellation_token: true,
            },
        )
        .await;

        sess.abort_all_tasks(TurnAbortReason::Interrupted).await;

        let evt = rx.recv().await.expect("event");
        match evt.msg {
            EventMsg::TurnAborted(e) => assert_eq!(TurnAbortReason::Interrupted, e.reason),
            other => panic!("unexpected event: {other:?}"),
        }
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn abort_review_task_emits_exited_then_aborted_and_records_history() {
        let (sess, tc, rx) = make_session_and_context_with_rx().await;
        let input = vec![UserInput::Text {
            text: "start review".to_string(),
        }];
        sess.spawn_task(Arc::clone(&tc), input, ReviewTask::new())
            .await;

        sess.abort_all_tasks(TurnAbortReason::Interrupted).await;

        // Drain events until we observe ExitedReviewMode; earlier
        // RawResponseItem entries (e.g., environment context) may arrive first.
        loop {
            let evt = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
                .await
                .expect("timeout waiting for first event")
                .expect("first event");
            match evt.msg {
                EventMsg::ExitedReviewMode(ev) => {
                    assert!(ev.review_output.is_none());
                    break;
                }
                // Ignore any non-critical events before exit.
                _ => continue,
            }
        }
        loop {
            let evt = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
                .await
                .expect("timeout waiting for next event")
                .expect("event");
            match evt.msg {
                EventMsg::RawResponseItem(_) => continue,
                EventMsg::ItemStarted(_) | EventMsg::ItemCompleted(_) => continue,
                EventMsg::AgentMessage(_) => continue,
                EventMsg::TurnAborted(e) => {
                    assert_eq!(TurnAbortReason::Interrupted, e.reason);
                    break;
                }
                other => panic!("unexpected second event: {other:?}"),
            }
        }

        let history = sess.clone_history().await.get_history();
        let _ = history;
    }

    #[tokio::test]
    async fn fatal_tool_error_stops_turn_and_reports_error() {
        let (session, turn_context, _rx) = make_session_and_context_with_rx().await;
        let tools = {
            session
                .services
                .mcp_connection_manager
                .read()
                .await
                .list_all_tools()
                .await
        };
        let router = ToolRouter::from_config(
            &turn_context.tools_config,
            Some(
                tools
                    .into_iter()
                    .map(|(name, tool)| (name, tool.tool))
                    .collect(),
            ),
        );
        let item = ResponseItem::CustomToolCall {
            id: None,
            status: None,
            call_id: "call-1".to_string(),
            name: "shell".to_string(),
            input: "{}".to_string(),
        };

        let call = ToolRouter::build_tool_call(session.as_ref(), item.clone())
            .await
            .expect("build tool call")
            .expect("tool call present");
        let tracker = Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new()));
        let err = router
            .dispatch_tool_call(
                Arc::clone(&session),
                Arc::clone(&turn_context),
                tracker,
                call,
            )
            .await
            .expect_err("expected fatal error");

        match err {
            FunctionCallError::Fatal(message) => {
                assert_eq!(message, "tool shell invoked with incompatible payload");
            }
            other => panic!("expected FunctionCallError::Fatal, got {other:?}"),
        }
    }

    fn sample_rollout(
        session: &Session,
        turn_context: &TurnContext,
    ) -> (Vec<RolloutItem>, Vec<ResponseItem>) {
        let mut rollout_items = Vec::new();
        let mut live_history = ContextManager::new();

        let initial_context = session.build_initial_context(turn_context);
        for item in &initial_context {
            rollout_items.push(RolloutItem::ResponseItem(item.clone()));
        }
        live_history.record_items(initial_context.iter(), turn_context.truncation_policy);

        let user1 = ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: "first user".to_string(),
            }],
        };
        live_history.record_items(std::iter::once(&user1), turn_context.truncation_policy);
        rollout_items.push(RolloutItem::ResponseItem(user1.clone()));

        let assistant1 = ResponseItem::Message {
            id: None,
            role: "assistant".to_string(),
            content: vec![ContentItem::OutputText {
                text: "assistant reply one".to_string(),
            }],
        };
        live_history.record_items(std::iter::once(&assistant1), turn_context.truncation_policy);
        rollout_items.push(RolloutItem::ResponseItem(assistant1.clone()));

        let summary1 = "summary one";
        let snapshot1 = live_history.get_history();
        let user_messages1 = collect_user_messages(&snapshot1);
        let rebuilt1 = compact::build_compacted_history(
            session.build_initial_context(turn_context),
            &user_messages1,
            summary1,
        );
        live_history.replace(rebuilt1);
        rollout_items.push(RolloutItem::Compacted(CompactedItem {
            message: summary1.to_string(),
            replacement_history: None,
        }));

        let user2 = ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: "second user".to_string(),
            }],
        };
        live_history.record_items(std::iter::once(&user2), turn_context.truncation_policy);
        rollout_items.push(RolloutItem::ResponseItem(user2.clone()));

        let assistant2 = ResponseItem::Message {
            id: None,
            role: "assistant".to_string(),
            content: vec![ContentItem::OutputText {
                text: "assistant reply two".to_string(),
            }],
        };
        live_history.record_items(std::iter::once(&assistant2), turn_context.truncation_policy);
        rollout_items.push(RolloutItem::ResponseItem(assistant2.clone()));

        let summary2 = "summary two";
        let snapshot2 = live_history.get_history();
        let user_messages2 = collect_user_messages(&snapshot2);
        let rebuilt2 = compact::build_compacted_history(
            session.build_initial_context(turn_context),
            &user_messages2,
            summary2,
        );
        live_history.replace(rebuilt2);
        rollout_items.push(RolloutItem::Compacted(CompactedItem {
            message: summary2.to_string(),
            replacement_history: None,
        }));

        let user3 = ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: "third user".to_string(),
            }],
        };
        live_history.record_items(std::iter::once(&user3), turn_context.truncation_policy);
        rollout_items.push(RolloutItem::ResponseItem(user3.clone()));

        let assistant3 = ResponseItem::Message {
            id: None,
            role: "assistant".to_string(),
            content: vec![ContentItem::OutputText {
                text: "assistant reply three".to_string(),
            }],
        };
        live_history.record_items(std::iter::once(&assistant3), turn_context.truncation_policy);
        rollout_items.push(RolloutItem::ResponseItem(assistant3.clone()));

        (rollout_items, live_history.get_history())
    }

    #[tokio::test]
    async fn rejects_escalated_permissions_when_policy_not_on_request() {
        use crate::exec::ExecParams;
        use crate::protocol::AskForApproval;
        use crate::protocol::SandboxPolicy;
        use crate::sandboxing::SandboxPermissions;
        use crate::turn_diff_tracker::TurnDiffTracker;
        use std::collections::HashMap;

        let (session, mut turn_context_raw) = make_session_and_context().await;
        // Ensure policy is NOT OnRequest so the early rejection path triggers
        turn_context_raw.approval_policy = AskForApproval::OnFailure;
        let session = Arc::new(session);
        let mut turn_context = Arc::new(turn_context_raw);

        let timeout_ms = 1000;
        let sandbox_permissions = SandboxPermissions::RequireEscalated;
        let params = ExecParams {
            command: if cfg!(windows) {
                vec![
                    "cmd.exe".to_string(),
                    "/C".to_string(),
                    "echo hi".to_string(),
                ]
            } else {
                vec![
                    "/bin/sh".to_string(),
                    "-c".to_string(),
                    "echo hi".to_string(),
                ]
            },
            cwd: turn_context.cwd.clone(),
            expiration: timeout_ms.into(),
            env: HashMap::new(),
            sandbox_permissions,
            justification: Some("test".to_string()),
            arg0: None,
        };

        let params2 = ExecParams {
            sandbox_permissions: SandboxPermissions::UseDefault,
            command: params.command.clone(),
            cwd: params.cwd.clone(),
            expiration: timeout_ms.into(),
            env: HashMap::new(),
            justification: params.justification.clone(),
            arg0: None,
        };

        let turn_diff_tracker = Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new()));

        let tool_name = "shell";
        let call_id = "test-call".to_string();

        let handler = ShellHandler;
        let resp = handler
            .handle(ToolInvocation {
                session: Arc::clone(&session),
                turn: Arc::clone(&turn_context),
                tracker: Arc::clone(&turn_diff_tracker),
                call_id,
                tool_name: tool_name.to_string(),
                payload: ToolPayload::Function {
                    arguments: serde_json::json!({
                        "command": params.command.clone(),
                        "workdir": Some(turn_context.cwd.to_string_lossy().to_string()),
                        "timeout_ms": params.expiration.timeout_ms(),
                        "sandbox_permissions": params.sandbox_permissions,
                        "justification": params.justification.clone(),
                    })
                    .to_string(),
                },
            })
            .await;

        let Err(FunctionCallError::RespondToModel(output)) = resp else {
            panic!("expected error result");
        };

        let expected = format!(
            "approval policy is {policy:?}; reject command — you should not ask for escalated permissions if the approval policy is {policy:?}",
            policy = turn_context.approval_policy
        );

        pretty_assertions::assert_eq!(output, expected);

        // Now retry the same command WITHOUT escalated permissions; should succeed.
        // Force DangerFullAccess to avoid platform sandbox dependencies in tests.
        Arc::get_mut(&mut turn_context)
            .expect("unique turn context Arc")
            .sandbox_policy = SandboxPolicy::DangerFullAccess;

        let resp2 = handler
            .handle(ToolInvocation {
                session: Arc::clone(&session),
                turn: Arc::clone(&turn_context),
                tracker: Arc::clone(&turn_diff_tracker),
                call_id: "test-call-2".to_string(),
                tool_name: tool_name.to_string(),
                payload: ToolPayload::Function {
                    arguments: serde_json::json!({
                        "command": params2.command.clone(),
                        "workdir": Some(turn_context.cwd.to_string_lossy().to_string()),
                        "timeout_ms": params2.expiration.timeout_ms(),
                        "sandbox_permissions": params2.sandbox_permissions,
                        "justification": params2.justification.clone(),
                    })
                    .to_string(),
                },
            })
            .await;

        let output = match resp2.expect("expected Ok result") {
            ToolOutput::Function { content, .. } => content,
            _ => panic!("unexpected tool output"),
        };

        #[derive(Deserialize, PartialEq, Eq, Debug)]
        struct ResponseExecMetadata {
            exit_code: i32,
        }

        #[derive(Deserialize)]
        struct ResponseExecOutput {
            output: String,
            metadata: ResponseExecMetadata,
        }

        let exec_output: ResponseExecOutput =
            serde_json::from_str(&output).expect("valid exec output json");

        pretty_assertions::assert_eq!(exec_output.metadata, ResponseExecMetadata { exit_code: 0 });
        assert!(exec_output.output.contains("hi"));
    }
    #[tokio::test]
    async fn unified_exec_rejects_escalated_permissions_when_policy_not_on_request() {
        use crate::protocol::AskForApproval;
        use crate::sandboxing::SandboxPermissions;
        use crate::turn_diff_tracker::TurnDiffTracker;

        let (session, mut turn_context_raw) = make_session_and_context().await;
        turn_context_raw.approval_policy = AskForApproval::OnFailure;
        let session = Arc::new(session);
        let turn_context = Arc::new(turn_context_raw);
        let tracker = Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new()));

        let handler = UnifiedExecHandler;
        let resp = handler
            .handle(ToolInvocation {
                session: Arc::clone(&session),
                turn: Arc::clone(&turn_context),
                tracker: Arc::clone(&tracker),
                call_id: "exec-call".to_string(),
                tool_name: "exec_command".to_string(),
                payload: ToolPayload::Function {
                    arguments: serde_json::json!({
                        "cmd": "echo hi",
                        "sandbox_permissions": SandboxPermissions::RequireEscalated,
                        "justification": "need unsandboxed execution",
                    })
                    .to_string(),
                },
            })
            .await;

        let Err(FunctionCallError::RespondToModel(output)) = resp else {
            panic!("expected error result");
        };

        let expected = format!(
            "approval policy is {policy:?}; reject command — you cannot ask for escalated permissions if the approval policy is {policy:?}",
            policy = turn_context.approval_policy
        );

        pretty_assertions::assert_eq!(output, expected);
    }
}
