<template>
    <div id="app">
        <h1>Typst Editor with LSP</h1>
        <div class="status">
            <span :class="['status-indicator', statusClass]"></span>
            LSP Status: {{ lspStatus }}
        </div>
        <div id="editor" ref="editorContainer"></div>
    </div>
</template>

<script setup>
import { ref, onMounted, onUnmounted } from 'vue'
import * as monaco from 'monaco-editor'

const editorContainer = ref(null)
const lspStatus = ref('initializing')
const statusClass = ref('status-connecting')

let editor = null
let worker = null
let requestId = 0
let documentUri = 'file:///example.typ'

// 创建 LSP 请求
function createLSPRequest(method, params = {}) {
    return {
        jsonrpc: '2.0',
        id: ++requestId,
        method,
        params
    }
}

// 发送 LSP 通知（无需响应）
function sendLSPNotification(method, params = {}) {
    if (worker) {
        worker.postMessage({
            jsonrpc: '2.0',
            method,
            params
        })
    }
}

// 发送 LSP 请求并等待响应
function sendLSPRequest(method, params = {}) {
    return new Promise((resolve, reject) => {
        if (!worker) {
            reject(new Error('Worker not available'))
            return
        }

        const request = createLSPRequest(method, params)
        const timeoutId = setTimeout(() => {
            reject(new Error('Request timeout'))
        }, 5000)

        const handler = (event) => {
            if (event.data.id === request.id) {
                clearTimeout(timeoutId)
                worker.removeEventListener('message', handler)

                if (event.data.error) {
                    reject(new Error(event.data.error.message))
                } else {
                    resolve(event.data.result)
                }
            }
        }

        worker.addEventListener('message', handler)
        worker.postMessage(request)
    })
}

// 初始化 LSP Worker
async function initializeLSP() {
    try {
        worker = new Worker(new URL('./lsp-worker.js', import.meta.url), { type: 'module' })

        worker.onmessage = (event) => {
            if (event.data.type === 'status' && event.data.status === 'ready') {
                lspStatus.value = 'ready'
                statusClass.value = 'status-ready'
                initializeLSPServer()
            }
        }

        worker.onerror = (error) => {
            console.error('Worker error:', error)
            lspStatus.value = 'error'
            statusClass.value = 'status-error'
        }

    } catch (error) {
        console.error('Failed to create worker:', error)
        lspStatus.value = 'error'
        statusClass.value = 'status-error'
    }
}

// 初始化 LSP 服务器
async function initializeLSPServer() {
    try {
        const initParams = {
            processId: null,
            clientInfo: { name: 'Monaco Typst Editor' },
            rootUri: null,
            capabilities: {
                textDocument: {
                    completion: {
                        completionItem: {
                            snippetSupport: true
                        }
                    }
                }
            }
        }

        await sendLSPRequest('initialize', initParams)
        lspStatus.value = 'initialized'
        statusClass.value = 'status-connected'

        // 打开文档
        openDocument()
    } catch (error) {
        console.error('LSP initialization failed:', error)
        lspStatus.value = 'init failed'
        statusClass.value = 'status-error'
    }
}

// 打开文档
function openDocument() {
    const content = editor.getValue()
    sendLSPNotification('textDocument/didOpen', {
        textDocument: {
            uri: documentUri,
            languageId: 'typst',
            version: 1,
            text: content
        }
    })
}

// 获取补全建议
async function getCompletions(position) {
    try {
        const result = await sendLSPRequest('textDocument/completion', {
            textDocument: { uri: documentUri },
            position: {
                line: position.lineNumber - 1, // Monaco 使用 1-based，LSP 使用 0-based
                character: position.column - 1
            }
        })
        return result
    } catch (error) {
        console.error('Completion failed:', error)
        return null
    }
}

// 初始化 Monaco Editor
function initializeEditor() {
    // 注册 Typst 语言
    monaco.languages.register({ id: 'typst' })

    // 设置基本语法高亮
    monaco.languages.setMonarchTokensProvider('typst', {
        tokenizer: {
            root: [
                [/#[a-zA-Z_][a-zA-Z0-9_]*/, 'keyword'],
                [/\*[^*]*\*/, 'string'],
                [/_[^_]*_/, 'string.italic'],
                [/\/\/.*$/, 'comment'],
                [/\/\*[\s\S]*?\*\//, 'comment'],
            ]
        }
    })

    // 创建编辑器
    editor = monaco.editor.create(editorContainer.value, {
        value: '= Hello Typst\n\nThis is a *bold* text.\n\n#lorem(50)',
        language: 'typst',
        theme: 'vs-dark',
        automaticLayout: true,
        minimap: { enabled: false },
        fontSize: 14,
    })

    // 注册补全提供者
    monaco.languages.registerCompletionItemProvider('typst', {
        provideCompletionItems: async (model, position) => {
            const completions = await getCompletions(position)

            if (!completions || !completions.items) {
                return { suggestions: [] }
            }

            const suggestions = completions.items.map(item => ({
                label: item.label,
                kind: monaco.languages.CompletionItemKind.Function,
                insertText: item.insertText || item.label,
                detail: item.detail,
                documentation: item.documentation
            }))

            return { suggestions }
        }
    })
}

onMounted(() => {
    initializeEditor()
    initializeLSP()
})

onUnmounted(() => {
    if (worker) {
        // 关闭文档
        sendLSPNotification('textDocument/didClose', {
            textDocument: { uri: documentUri }
        })
        worker.terminate()
    }
    if (editor) {
        editor.dispose()
    }
})
</script>

<style>
#app {
    height: 100vh;
    display: flex;
    flex-direction: column;
    font-family: Arial, sans-serif;
}

h1 {
    margin: 0;
    padding: 1rem;
    background: #1e1e1e;
    color: white;
    text-align: center;
}

.status {
    padding: 0.5rem 1rem;
    background: #2d2d30;
    color: white;
    display: flex;
    align-items: center;
    gap: 0.5rem;
}

.status-indicator {
    width: 8px;
    height: 8px;
    border-radius: 50%;
}

.status-connecting {
    background-color: #ffa500;
}

.status-ready {
    background-color: #90EE90;
}

.status-connected {
    background-color: #00ff00;
}

.status-error {
    background-color: #ff0000;
}

#editor {
    flex: 1;
    min-height: 0;
}
</style>
