// lsp-worker.js (作为模块)
import init, { TinymistLanguageServer } from "tinymist";
await init();
const server = new TinymistLanguageServer()
let initialized = false

// 发送响应
function sendResponse(id, result) {
    self.postMessage({
        jsonrpc: '2.0',
        id,
        result
    })
}

// 发送错误
function sendError(id, message) {
    self.postMessage({
        jsonrpc: '2.0',
        id,
        error: { code: -1, message }
    })
}

self.onmessage = async function (event) {
    const { method, params, id } = event.data

    try {
        switch (method) {
            case 'initialize':
                const result = server.on_request('initialize', params)
                initialized = true
                sendResponse(id, result)
                await server.on_notification('initialized', {})
                break

            case 'textDocument/didOpen':
                if (!initialized) {
                    sendError(id, 'Server not initialized')
                    return
                }
                await server.on_notification('textDocument/didOpen', params)
                break

            case 'textDocument/didClose':
                if (!initialized) {
                    sendError(id, 'Server not initialized')
                    return
                }
                await server.on_notification('textDocument/didClose', params)
                break

            case 'textDocument/completion':
                if (!initialized) {
                    sendError(id, 'Server not initialized')
                    return
                }
                const completions = server.on_request('textDocument/completion', params)
                sendResponse(id, completions)
                break

            default:
                if (id) {
                    sendError(id, `Unknown method: ${method}`)
                }
        }
    } catch (error) {
        console.error('LSP error:', error)
        if (id) {
            sendError(id, error.message)
        }
    }
}

self.postMessage({
    type: 'status',
    status: 'ready'
})

console.log('Tinymist LSP Worker started')
