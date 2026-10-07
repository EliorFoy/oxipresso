# Oxipresso for Zed

**中文**：Zed 编辑器的 [Oxipresso](https://github.com/EliorFoy/oxipresso) 扩展——为 LaTeX 文档注册 `oxipresso-lsp` 语言服务器：按键即增量重排，TeX 错误以诊断形式直接显示在编辑器里，产物与 SyncTeX sidecar 由伴随预览（Oxipresso Editor Client）实时渲染并跟随编辑位置翻页。

**English**: The [Oxipresso](https://github.com/EliorFoy/oxipresso) extension for the Zed editor — registers the `oxipresso-lsp` language server for LaTeX buffers: every keystroke drives an incremental re-typeset, TeX errors appear as editor diagnostics, and the artifact / SyncTeX sidecar are rendered live by the companion preview (the Oxipresso Editor Client), which follows your edit position page by page.

## 安装 / Install

1. 构建并安装 Oxipresso 渲染服务器（`oxipresso`、`oxipresso-lsp`，见主仓库）/ Build and install the Oxipresso render server (see the main repository).
2. 把 `oxipresso-lsp` 放入 PATH，或设置 `OXIPRESSO_LSP` 环境变量指向其完整路径 / Put `oxipresso-lsp` on PATH, or point `OXIPRESSO_LSP` at it.
3. Zed → Extensions → Install Dev Extension → 选择本目录 / Install this directory as a dev extension.

## 许可证 / License

MIT。基于 [TeXpresso](https://github.com/let-def/texpresso)（MIT）的编辑器集成思路。
