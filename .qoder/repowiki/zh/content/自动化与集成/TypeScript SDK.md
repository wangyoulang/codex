# TypeScript SDK

<cite>
**本文档中引用的文件**  
- [package.json](file://sdk/typescript/package.json)
- [index.ts](file://sdk/typescript/src/index.ts)
- [codex.ts](file://sdk/typescript/src/codex.ts)
- [thread.ts](file://sdk/typescript/src/thread.ts)
- [codexOptions.ts](file://sdk/typescript/src/codexOptions.ts)
- [threadOptions.ts](file://sdk/typescript/src/threadOptions.ts)
- [turnOptions.ts](file://sdk/typescript/src/turnOptions.ts)
- [events.ts](file://sdk/typescript/src/events.ts)
- [exec.ts](file://sdk/typescript/src/exec.ts)
- [items.ts](file://sdk/typescript/src/items.ts)
- [outputSchemaFile.ts](file://sdk/typescript/src/outputSchemaFile.ts)
- [README.md](file://sdk/typescript/README.md)
- [basic_streaming.ts](file://sdk/typescript/samples/basic_streaming.ts)
- [structured_output_zod.ts](file://sdk/typescript/samples/structured_output_zod.ts)
- [run.test.ts](file://sdk/typescript/tests/run.test.ts)
- [runStreamed.test.ts](file://sdk/typescript/tests/runStreamed.test.ts)
</cite>

## 目录
1. [安装](#安装)
2. [核心类 `Codex`](#核心类-codex)
3. [`Thread` 类的使用](#thread-类的使用)
4. [配置选项](#配置选项)
5. [错误处理与事件监听](#错误处理与事件监听)
6. [高级示例：使用 Zod 进行结构化输出](#高级示例使用-zod-进行结构化输出)

## 安装

Codex TypeScript SDK 可以通过 npm 安装，命令如下：

```bash
npm install @openai/codex-sdk
```

该 SDK 要求 Node.js 版本为 18 或更高。

**Section sources**
- [package.json](file://sdk/typescript/package.json#L2)
- [README.md](file://sdk/typescript/README.md#L10)

## 核心类 `Codex`

`Codex` 类是与 Codex 代理交互的主要入口点。它提供了启动新会话和恢复现有会话的方法。

### 构造函数参数

`Codex` 类的构造函数接受一个可选的 `CodexOptions` 对象作为参数，其定义如下：

```typescript
type CodexOptions = {
  codexPathOverride?: string;
  baseUrl?: string;
  apiKey?: string;
  env?: Record<string, string>;
};
```

- `codexPathOverride`: 可选，用于指定 `codex` 可执行文件的路径。
- `baseUrl`: 可选，用于指定 API 的基础 URL。
- `apiKey`: 可选，用于认证的 API 密钥。
- `env`: 可选，传递给 Codex CLI 进程的环境变量。如果提供，SDK 将不会继承 `process.env` 中的变量。

**Section sources**
- [codexOptions.ts](file://sdk/typescript/src/codexOptions.ts#L1-L11)
- [codex.ts](file://sdk/typescript/src/codex.ts#L15-L18)

### 初始化 SDK

在 Node.js 应用中初始化 SDK 的示例如下：

```typescript
import { Codex } from "@openai/codex-sdk";

const codex = new Codex({
  baseUrl: "https://api.example.com",
  apiKey: "your-api-key",
});
```

**Section sources**
- [README.md](file://sdk/typescript/README.md#L20)

## `Thread` 类的使用

`Thread` 类代表与代理的一次会话。一个会话可以包含多个连续的回合（turn）。

### 创建会话

使用 `Codex` 实例的 `startThread` 方法可以启动一个新会话：

```typescript
const thread = codex.startThread();
```

### 发送消息

通过 `run` 方法向代理发送消息并获取完整的响应：

```typescript
const turn = await thread.run("诊断测试失败并提出修复方案");
console.log(turn.finalResponse);
console.log(turn.items);
```

### 处理流式响应

`run` 方法会缓冲事件直到回合完成。如果需要对中间进度（如工具调用、流式响应和文件更改通知）做出反应，应使用 `runStreamed` 方法，该方法返回一个异步生成器，用于生成结构化事件：

```typescript
const { events } = await thread.runStreamed("诊断测试失败并提出修复方案");

for await (const event of events) {
  switch (event.type) {
    case "item.completed":
      console.log("item", event.item);
      break;
    case "turn.completed":
      console.log("usage", event.usage);
      break;
  }
}
```

**Section sources**
- [thread.ts](file://sdk/typescript/src/thread.ts#L66-L137)
- [README.md](file://sdk/typescript/README.md#L39)

## 配置选项

### `ThreadOptions`

`ThreadOptions` 用于配置会话的选项，定义如下：

```typescript
type ThreadOptions = {
  model?: string;
  sandboxMode?: SandboxMode;
  workingDirectory?: string;
  skipGitRepoCheck?: boolean;
  modelReasoningEffort?: ModelReasoningEffort;
  networkAccessEnabled?: boolean;
  webSearchEnabled?: boolean;
  approvalPolicy?: ApprovalMode;
  additionalDirectories?: string[];
};
```

### `TurnOptions`

`TurnOptions` 用于配置单个回合的选项，定义如下：

```typescript
type TurnOptions = {
  outputSchema?: unknown;
  signal?: AbortSignal;
};
```

- `outputSchema`: 描述期望代理输出的 JSON 模式。
- `signal`: 用于取消回合的 `AbortSignal`。

**Section sources**
- [threadOptions.ts](file://sdk/typescript/src/threadOptions.ts#L7-L17)
- [turnOptions.ts](file://sdk/typescript/src/turnOptions.ts#L1-L7)

## 错误处理与事件监听

SDK 提供了详细的事件系统，用于监听会话中的各种事件。主要事件类型包括：

- `thread.started`: 新会话开始时发出。
- `turn.started`: 新回合开始时发出。
- `item.completed`: 项目完成时发出。
- `turn.completed`: 回合完成时发出。
- `turn.failed`: 回合失败时发出。

错误处理通过捕获 `turn.failed` 事件或 `run` 方法抛出的异常来实现。

**Section sources**
- [events.ts](file://sdk/typescript/src/events.ts#L6-L81)

## 高级示例：使用 Zod 进行结构化输出

Codex 代理可以生成符合指定模式的 JSON 响应。可以使用 [Zod](https://github.com/colinhacks/zod) 模式并结合 [`zod-to-json-schema`](https://www.npmjs.com/package/zod-to-json-schema) 包来创建 JSON 模式：

```typescript
import z from "zod";
import zodToJsonSchema from "zod-to-json-schema";

const schema = z.object({
  summary: z.string(),
  status: z.enum(["ok", "action_required"]),
});

const turn = await thread.run("总结仓库状态", {
  outputSchema: zodToJsonSchema(schema, { target: "openAi" }),
});
console.log(turn.finalResponse);
```

**Section sources**
- [structured_output_zod.ts](file://sdk/typescript/samples/structured_output_zod.ts#L11-L18)
- [README.md](file://sdk/typescript/README.md#L75)