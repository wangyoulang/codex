use serde::Serialize;
use tracing::error;
use tracing::warn;

#[derive(Debug, Default)]
pub(crate) struct UserNotifier {
    notify_command: Option<Vec<String>>,
}

impl UserNotifier {
    pub(crate) fn notify(&self, notification: &UserNotification) {
        if let Some(notify_command) = &self.notify_command
            && !notify_command.is_empty()
        {
            self.invoke_notify(notify_command, notification)
        }
    }

    fn invoke_notify(&self, notify_command: &[String], notification: &UserNotification) {
        // 把通知结构体序列化成 JSON 字符串，作为外部通知程序的参数传递。
        let Ok(json) = serde_json::to_string(&notification) else {
            error!("failed to serialise notification payload");
            return;
        };

        // notify_command 是一个命令数组：第 0 个元素是可执行文件，其余是固定参数。
        let mut command = std::process::Command::new(&notify_command[0]);
        if notify_command.len() > 1 {
            // 追加用户配置的固定参数（不包含最后的 JSON 负载）。
            command.args(&notify_command[1..]);
        }
        // 最后再追加本次通知的 JSON 负载。
        command.arg(json);

        // “发出去就算”：不等待通知程序执行完成，避免阻塞主流程。
        if let Err(e) = command.spawn() {
            // 只记录警告，不向上抛错；通知失败不应影响核心对话流程。
            warn!("failed to spawn notifier '{}': {e}", notify_command[0]);
        }
    }

    pub(crate) fn new(notify: Option<Vec<String>>) -> Self {
        Self {
            notify_command: notify,
        }
    }
}

/// User can configure a program that will receive notifications. Each
/// notification is serialized as JSON and passed as an argument to the
/// program.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub(crate) enum UserNotification {
    #[serde(rename_all = "kebab-case")]
    AgentTurnComplete {
        thread_id: String,
        turn_id: String,
        cwd: String,

        /// Messages that the user sent to the agent to initiate the turn.
        input_messages: Vec<String>,

        /// The last message sent by the assistant in the turn.
        last_assistant_message: Option<String>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Result;

    #[test]
    fn test_user_notification() -> Result<()> {
        let notification = UserNotification::AgentTurnComplete {
            thread_id: "b5f6c1c2-1111-2222-3333-444455556666".to_string(),
            turn_id: "12345".to_string(),
            cwd: "/Users/example/project".to_string(),
            input_messages: vec!["Rename `foo` to `bar` and update the callsites.".to_string()],
            last_assistant_message: Some(
                "Rename complete and verified `cargo build` succeeds.".to_string(),
            ),
        };
        let serialized = serde_json::to_string(&notification)?;
        assert_eq!(
            serialized,
            r#"{"type":"agent-turn-complete","thread-id":"b5f6c1c2-1111-2222-3333-444455556666","turn-id":"12345","cwd":"/Users/example/project","input-messages":["Rename `foo` to `bar` and update the callsites."],"last-assistant-message":"Rename complete and verified `cargo build` succeeds."}"#
        );
        Ok(())
    }
}
