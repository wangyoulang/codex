# MCP服务器模式

<cite>
**本文档引用的文件**
- [main.rs](file://codex-rs/mcp-server/src/main.rs)
- [lib.rs](file://codex-rs/mcp-server/src/lib.rs)
- [message_processor.rs](file://codex-rs/mcp-server/src/message_processor.rs)
- [codex_tool_runner.rs](file://codex-rs/mcp-server/src/codex_tool_runner.rs)
- [codex_tool_config.rs](file://codex-rs/mcp-server/src/codex_tool_config.rs)
- [outgoing_message.rs](file://codex-rs/mcp-server/src/outgoing_message.rs)
- [mcp_cmd.rs](file://codex-rs/cli/src/mcp_cmd.rs)
- [types.rs](file://codex-rs/core/src/config/types.rs)
</cite>

## 目录
1. [简介](#简介)
2. [启动MCP服务器](#启动mcp服务器)
3. [服务器连接协议](#服务器连接协议)
4. [工具管理与调用](#工具管理与调用)
5. [客户端连接示例](#客户端连接示例)
6. [生命周期、错误处理与日志](#生命周期错误处理与日志)

## 简介
`codex mcp-server` 命令用于启动一个Model Context Protocol (MCP) 服务器，该服务器作为客户端（如LLM）与Codex功能之间的桥梁。此服务器通过JSON-RPC协议接收来自客户端的请求，处理工具调用，并将结果返回。本文档详细说明了如何启动服务器、其通信协议、工具管理机制以及客户端交互方式。

## 启动MCP服务器
`codex mcp-server` 命令本身不直接接受 `--port` 或 `--host` 这样的命令行参数。相反，MCP服务器主要通过标准输入/输出（stdio）进行通信，这使其成为一个无头（headless）进程，通常由另一个管理进程（如 `codex mcp` CLI）启动和管理。

MCP服务器的配置和启动是通过 `codex mcp` 子命令来完成的。用户使用 `codex mcp add` 命令将MCP服务器的启动信息添加到全局配置中。该命令的核心参数是 `--command`，它指定了启动MCP服务器的完整命令。

例如，要添加一个名为 `my-codex-server` 的MCP服务器配置，可以使用以下命令：
```bash
codex mcp add my-codex-server -- codex mcp-server
```
在此命令中：
- `my-codex-server` 是为该服务器配置指定的名称。
- `--` 之后的部分是 `--command` 参数，即 `codex mcp-server`，这是启动MCP服务器二进制文件的实际命令。

此外，`add` 命令还支持 `--env` 参数，用于在启动服务器时设置环境变量。

**Section sources**
- [mcp_cmd.rs](file://codex-rs/cli/src/mcp_cmd.rs#L64-L239)
- [types.rs](file://codex-rs/core/src/config/types.rs#L76-L897)

## 服务器连接协议
MCP服务器主要通过 **标准输入/输出 (stdio)** 与客户端进行通信。这是一种基于文本的、逐行的JSON-RPC 2.0协议。服务器从标准输入读取JSON格式的请求和通知，并将响应、通知和错误写入标准输出。

### 通信流程
1.  **初始化 (Initialize)**: 客户端首先发送一个 `initialize` 请求，其中包含客户端信息和功能。服务器必须响应一个 `InitializeResult`，声明其支持的功能。服务器在初始化后，不能再接受另一个 `initialize` 请求。
2.  **工具发现 (List Tools)**: 客户端可以发送 `tools/list` 请求来查询服务器支持的工具。服务器会返回一个包含所有可用工具及其输入/输出模式的列表。
3.  **工具调用 (Call Tool)**: 客户端通过发送 `tools/call` 请求来调用特定工具。请求中包含工具名称和参数。
4.  **事件流 (Event Streaming)**: 在工具执行期间，服务器会通过 `codex/event` 通知向客户端发送一系列事件，以提供执行进度和中间结果。
5.  **响应与完成**: 工具执行完成后，服务器会发送一个 `tools/call` 响应，其中包含最终结果。如果客户端需要取消一个正在进行的调用，它可以发送一个 `notifications/cancelled` 通知。

### 认证机制
MCP服务器本身不直接处理OAuth等认证。认证主要在 **流式HTTP (streamable HTTP)** 类型的MCP服务器配置中处理。当用户使用 `codex mcp add` 添加一个HTTP类型的服务器时，CLI工具会自动检测该服务器是否支持OAuth，并在必要时启动OAuth登录流程。对于通过stdio运行的 `codex mcp-server`，认证通常由启动它的父进程或环境变量（如 `--env` 参数设置的）来处理。

**Section sources**
- [lib.rs](file://codex-rs/mcp-server/src/lib.rs#L58-L148)
- [message_processor.rs](file://codex-rs/mcp-server/src/message_processor.rs#L177-L229)
- [message_processor.rs](file://codex-rs/mcp-server/src/message_processor.rs#L299-L315)

## 工具管理与调用
MCP服务器的核心功能是管理和执行工具。服务器内置了两个主要工具：`codex` 和 `codex-reply`。

### 内置工具
1.  **`codex` 工具**: 这是主要的工具，用于启动一个新的Codex会话。当客户端调用此工具时，它会传递一个包含初始提示和配置参数的JSON对象。服务器会根据这些参数创建一个新的 `CodexConversation` 实例并开始执行。
2.  **`codex-reply` 工具**: 此工具用于向一个已存在的Codex会话发送后续提示。它需要会话的ID和新的用户提示。

### 工具加载与路由
服务器在 `message_processor.rs` 文件中定义了 `MessageProcessor` 结构体，它负责接收和分发所有传入的JSON-RPC消息。

- **工具列表**: 当收到 `tools/list` 请求时，`MessageProcessor` 会调用 `handle_list_tools` 方法。该方法会返回一个包含 `codex` 和 `codex-reply` 工具的列表。每个工具的定义都包含了其名称、标题、描述和详细的JSON Schema，用于验证输入参数。
- **工具调用路由**: 当收到 `tools/call` 请求时，`MessageProcessor` 会调用 `handle_call_tool` 方法。该方法会检查请求中的工具名称，并根据名称将调用路由到相应的处理函数（`handle_tool_call_codex` 或 `handle_tool_call_codex_session_reply`）。

### 工具执行
工具的执行是异步的。当 `handle_tool_call_codex` 被调用时，它会：
1.  解析传入的JSON参数，将其转换为 `CodexToolCallParam` 结构体。
2.  根据参数创建一个 `Config` 对象，用于配置新的Codex会话。
3.  使用 `tokio::spawn` 创建一个新的异步任务来运行会话，从而避免阻塞主消息处理循环。
4.  新任务调用 `run_codex_tool_session` 函数，该函数会启动 `CodexConversation`，提交初始提示，并持续监听来自会话的事件。

**Section sources**
- [message_processor.rs](file://codex-rs/mcp-server/src/message_processor.rs#L300-L345)
- [codex_tool_config.rs](file://codex-rs/mcp-server/src/codex_tool_config.rs#L107-L136)
- [codex_tool_runner.rs](file://codex-rs/mcp-server/src/codex_tool_runner.rs#L39-L114)

## 客户端连接示例
以下是一个使用Python编写的简单客户端示例，它通过标准输入/输出与 `codex mcp-server` 进行通信。

```python
import subprocess
import json
import sys

def send_request(proc, method, params=None, request_id=1):
    """向MCP服务器发送一个JSON-RPC请求。"""
    request = {
        "jsonrpc": "2.0",
        "id": request_id,
        "method": method
    }
    if params is not None:
        request["params"] = params
    
    # 将请求序列化为JSON并写入标准输入
    proc.stdin.write(json.dumps(request) + "\n")
    proc.stdin.flush()

def read_message(proc):
    """从标准输出读取一行并解析为JSON-RPC消息。"""
    line = proc.stdout.readline()
    if not line:
        return None
    return json.loads(line.strip())

# 启动MCP服务器进程
proc = subprocess.Popen(
    ["codex", "mcp-server"],
    stdin=subprocess.PIPE,
    stdout=subprocess.PIPE,
    stderr=subprocess.PIPE,
    text=True,
    bufsize=1
)

try:
    # 1. 发送初始化请求
    send_request(proc, "initialize", {
        "clientInfo": {
            "name": "MyClient",
            "version": "1.0"
        },
        "protocolVersion": "2025-06-18",
        "capabilities": {}
    })

    # 读取初始化响应
    response = read_message(proc)
    print("初始化响应:", response)

    # 2. 列出可用工具
    send_request(proc, "tools/list")
    response = read_message(proc)
    print("工具列表:", response)

    # 3. 调用codex工具
    send_request(proc, "tools/call", {
        "name": "codex",
        "arguments": {
            "prompt": "Hello, world!"
        }
    }, request_id=2)

    # 4. 持续读取事件通知，直到收到最终响应
    while True:
        message = read_message(proc)
        if message is None:
            break
        
        if message.get("method") == "codex/event":
            print("事件:", message)
        elif "id" in message and message["id"] == 2:
            # 这是针对我们tools/call请求的响应
            print("工具调用结果:", message)
            break

finally:
    proc.terminate()
    proc.wait()
```

**Section sources**
- [lib.rs](file://codex-rs/mcp-server/src/lib.rs#L58-L82)
- [lib.rs](file://codex-rs/mcp-server/src/lib.rs#L121-L140)

## 生命周期、错误处理与日志
### 服务器生命周期
MCP服务器的生命周期由其标准输入的关闭来控制。当父进程关闭了写入到服务器标准输入的管道时，服务器的 `stdin_reader_handle` 任务会检测到EOF（文件结束），然后退出。这会触发主消息处理循环的关闭，进而导致所有其他任务（如处理器和输出写入器）优雅地终止。

### 错误处理
服务器在多个层面实现了错误处理：
- **消息解析错误**: 如果从标准输入读取的JSON无法被解析，服务器会在日志中记录一个错误，但不会向客户端发送响应，因为无法确定请求的ID。
- **无效请求**: 对于格式错误或不支持的请求（如重复的 `initialize`），服务器会发送一个带有特定错误代码（如 `INVALID_REQUEST_ERROR_CODE`）的JSON-RPC错误响应。
- **工具执行错误**: 在工具执行过程中发生的任何错误（如配置加载失败）都会被捕获，并通过 `tools/call` 响应返回给客户端，其中 `is_error` 字段被设置为 `true`。

### 日志记录
服务器使用 `tracing` 库进行日志记录。日志输出到标准错误流（stderr）。用户可以通过设置 `RUST_LOG` 环境变量来控制日志的级别和格式。例如，`RUST_LOG=info` 会显示信息级别的日志，而 `RUST_LOG=debug` 会显示更详细的调试信息。日志对于调试连接问题和理解服务器内部行为非常有用。

**Section sources**
- [lib.rs](file://codex-rs/mcp-server/src/lib.rs#L50-L55)
- [lib.rs](file://codex-rs/mcp-server/src/lib.rs#L143-L148)
- [message_processor.rs](file://codex-rs/mcp-server/src/message_processor.rs#L173-L175)
- [outgoing_message.rs](file://codex-rs/mcp-server/src/outgoing_message.rs#L132-L135)