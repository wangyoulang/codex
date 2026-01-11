# MCP工具开发

<cite>
**本文档中引用的文件**  
- [package.json](file://shell-tool-mcp/package.json)
- [index.ts](file://shell-tool-mcp/src/index.ts)
- [types.ts](file://shell-tool-mcp/src/types.ts)
- [bashSelection.ts](file://shell-tool-mcp/src/bashSelection.ts)
- [osRelease.ts](file://shell-tool-mcp/src/osRelease.ts)
- [constants.ts](file://shell-tool-mcp/src/constants.ts)
- [platform.ts](file://shell-tool-mcp/src/platform.ts)
- [bash-exec-wrapper.patch](file://shell-tool-mcp/patches/bash-exec-wrapper.patch)
- [lib.rs](file://codex-rs/mcp-types/src/lib.rs)
- [mcp-server/lib.rs](file://codex-rs/mcp-server/src/lib.rs)
</cite>

## 目录
1. [简介](#简介)
2. [项目结构](#项目结构)
3. [核心组件](#核心组件)
4. [架构概述](#架构概述)
5. [详细组件分析](#详细组件分析)
6. [依赖分析](#依赖分析)
7. [性能考虑](#性能考虑)
8. [故障排除指南](#故障排除指南)
9. [结论](#结论)

## 简介
本文档旨在为开发者提供一个全面的MCP（Model Context Protocol）工具开发指南。通过分析`shell-tool-mcp`示例项目，详细阐述如何使用TypeScript/JavaScript创建自定义MCP工具，包括工具定义、执行请求处理、跨平台兼容性实现、JSON Schema输入输出定义、工具打包部署以及运行时补丁应用等关键环节。

## 项目结构
`shell-tool-mcp`项目是一个专门用于实现shell工具功能的MCP服务器。其结构清晰地分离了源代码、测试、补丁和构建配置。

```mermaid
graph TD
A[shell-tool-mcp] --> B[patches]
A --> C[src]
A --> D[tests]
A --> E[package.json]
B --> F[bash-exec-wrapper.patch]
C --> G[index.ts]
C --> H[bashSelection.ts]
C --> I[osRelease.ts]
C --> J[constants.ts]
C --> K[platform.ts]
C --> L[types.ts]
D --> M[bashSelection.test.ts]
D --> N[osRelease.test.ts]
```

**Diagram sources**
- [package.json](file://shell-tool-mcp/package.json)
- [src/index.ts](file://shell-tool-mcp/src/index.ts)
- [patches/bash-exec-wrapper.patch](file://shell-tool-mcp/patches/bash-exec-wrapper.patch)

**Section sources**
- [package.json](file://shell-tool-mcp/package.json)
- [src/index.ts](file://shell-tool-mcp/src/index.ts)

## 核心组件
本项目的核心组件包括工具启动器、Bash选择器、操作系统信息读取器、平台目标三元组解析器以及类型定义。这些组件协同工作，确保MCP工具能够在不同操作系统和架构上正确运行。

**Section sources**
- [index.ts](file://shell-tool-mcp/src/index.ts)
- [bashSelection.ts](file://shell-tool-mcp/src/bashSelection.ts)
- [osRelease.ts](file://shell-tool-mcp/src/osRelease.ts)
- [platform.ts](file://shell-tool-mcp/src/platform.ts)
- [types.ts](file://shell-tool-mcp/src/types.ts)

## 架构概述
`shell-tool-mcp`的架构遵循MCP协议规范，通过JSON-RPC与客户端通信。它主要由以下几个部分组成：工具注册、请求处理、跨平台兼容性处理和运行时补丁应用。

```mermaid
graph LR
Client[客户端] --> |JSON-RPC请求| MCP[MCP服务器]
MCP --> Bash[选择Bash变体]
MCP --> OS[读取OS信息]
MCP --> Platform[解析平台三元组]
MCP --> Patch[应用运行时补丁]
MCP --> |执行结果| Client
```

**Diagram sources**
- [lib.rs](file://codex-rs/mcp-types/src/lib.rs)
- [mcp-server/lib.rs](file://codex-rs/mcp-server/src/lib.rs)

## 详细组件分析

### 工具定义与执行
在MCP工具开发中，`tools`数组用于定义可用的工具及其参数。`callTool`函数负责处理执行请求，根据传入的工具名称和参数调用相应的功能。

#### 工具定义
```mermaid
classDiagram
class Tool {
+name : string
+description : string
+inputSchema : JSONSchema
+outputSchema : JSONSchema
}
class JSONSchema {
+type : string
+properties : object
+required : string[]
}
Tool --> JSONSchema : "has"
```

**Diagram sources**
- [lib.rs](file://codex-rs/mcp-types/src/lib.rs)

#### 执行请求处理
```mermaid
sequenceDiagram
participant Client as "客户端"
participant MCP as "MCP服务器"
participant Tool as "工具处理器"
Client->>MCP : tools/call请求
MCP->>Tool : 调用callTool函数
Tool->>Tool : 验证输入参数
Tool->>Tool : 执行工具逻辑
Tool-->>MCP : 返回执行结果
MCP-->>Client : JSON-RPC响应
```

**Diagram sources**
- [mcp-server/lib.rs](file://codex-rs/mcp-server/src/lib.rs)

### 跨平台兼容性
为了确保工具在不同平台上的兼容性，项目实现了`bashSelection`和`osRelease`模块。

#### Bash选择机制
```mermaid
flowchart TD
Start([开始]) --> ReadOS["读取操作系统信息"]
ReadOS --> CheckPlatform["检查平台类型"]
CheckPlatform --> |Linux| SelectLinux["选择Linux Bash变体"]
CheckPlatform --> |Darwin| SelectDarwin["选择macOS Bash变体"]
SelectLinux --> FindMatch["查找匹配的Bash版本"]
SelectDarwin --> FindDarwin["根据Darwin版本选择"]
FindMatch --> ReturnPath["返回Bash路径"]
FindDarwin --> ReturnPath
ReturnPath --> End([结束])
```

**Diagram sources**
- [bashSelection.ts](file://shell-tool-mcp/src/bashSelection.ts)
- [osRelease.ts](file://shell-tool-mcp/src/osRelease.ts)

**Section sources**
- [bashSelection.ts](file://shell-tool-mcp/src/bashSelection.ts)
- [osRelease.ts](file://shell-tool-mcp/src/osRelease.ts)
- [constants.ts](file://shell-tool-mcp/src/constants.ts)

### JSON Schema定义
根据`mcp-types`中的定义，工具的输入和输出需要遵循严格的JSON Schema规范。

```mermaid
erDiagram
TOOL {
string name PK
string description
object inputSchema
object outputSchema
}
SCHEMA {
string type PK
object properties
array required
}
TOOL ||--o{ SCHEMA : "defines"
```

**Diagram sources**
- [lib.rs](file://codex-rs/mcp-types/src/lib.rs)

### 打包与部署
工具的打包和部署通过`package.json`中的脚本进行管理。

```mermaid
graph TB
Build[构建] --> |tsup| Compile["编译TypeScript"]
Compile --> Bundle["打包成JavaScript"]
Bundle --> Test["运行测试"]
Test --> Deploy["部署到NPM"]
```

**Diagram sources**
- [package.json](file://shell-tool-mcp/package.json)

### 运行时补丁应用
`patches`目录中的`bash-exec-wrapper.patch`用于在运行时修改Bash的行为。

```mermaid
graph LR
Original[原始Bash] --> |应用补丁| Patched["打补丁后的Bash"]
Patched --> |设置环境变量| Wrapper["BASH_EXEC_WRAPPER"]
Wrapper --> |执行命令| Modified["修改后的执行流程"]
```

**Diagram sources**
- [bash-exec-wrapper.patch](file://shell-tool-mcp/patches/bash-exec-wrapper.patch)

**Section sources**
- [bash-exec-wrapper.patch](file://shell-tool-mcp/patches/bash-exec-wrapper.patch)

## 依赖分析
项目依赖关系清晰，主要依赖Node.js内置模块和TypeScript开发工具。

```mermaid
graph TD
A[shell-tool-mcp] --> B[node:child_process]
A --> C[node:fs]
A --> D[node:os]
A --> E[node:path]
A --> F[tsup]
A --> G[jest]
A --> H[prettier]
```

**Diagram sources**
- [package.json](file://shell-tool-mcp/package.json)

**Section sources**
- [package.json](file://shell-tool-mcp/package.json)

## 性能考虑
由于MCP工具需要处理实时的JSON-RPC通信，性能优化至关重要。建议使用异步I/O操作，避免阻塞主线程，并合理利用缓存机制减少重复计算。

## 故障排除指南
常见问题包括Bash路径找不到、操作系统信息读取失败和跨平台兼容性问题。建议检查`/etc/os-release`文件是否存在，确认Bash变体配置是否正确，并验证平台三元组解析逻辑。

**Section sources**
- [osRelease.ts](file://shell-tool-mcp/src/osRelease.ts)
- [platform.ts](file://shell-tool-mcp/src/platform.ts)

## 结论
通过本指南，开发者可以全面了解MCP工具的开发流程，从工具定义到部署的各个环节都有详细的说明。`shell-tool-mcp`项目提供了一个优秀的参考实现，展示了如何构建一个健壮、跨平台的MCP工具。