-- Real native Neovim client; the Python launcher owns environment/process cleanup.
local api = vim.api
local workspace = assert(vim.env.E2E_WORKSPACE)
local logs = assert(vim.env.E2E_LOG_DIR)
local errors = {}
local completed = {}
local clients = {}

local function record(event, data)
  local file = assert(io.open(logs .. '/events.jsonl', 'a'))
  file:write(vim.json.encode({ event = event, data = data }), '\n')
  file:close()
end

local function check(condition, message)
  if not condition then
    error(message, 2)
  end
end

local notify = vim.notify
vim.notify = function(message, level, options)
  record('notify', { message = tostring(message), level = level })
  if level == vim.log.levels.ERROR then
    errors[#errors + 1] = tostring(message)
  end
  return notify(message, level, options)
end

local function healthy()
  check(#errors == 0, 'native editor/client error: ' .. table.concat(errors, '\n'))
end

local function wait_for(description, predicate)
  local ok = vim.wait(10000, function()
    healthy()
    return predicate()
  end, 10)
  healthy()
  check(ok, 'timeout waiting for ' .. description)
end

-- Observers ALWAYS invoke the native handler first. In particular, capability
-- registration must exercise Neovim's real watcher/type-hierarchy APIs.
local function observe(method, after)
  local native = assert(vim.lsp.handlers[method], 'missing native handler: ' .. method)
  return function(err, result, context, config)
    local values = { xpcall(function()
      return native(err, result, context, config)
    end, debug.traceback) }
    if not values[1] then
      errors[#errors + 1] = values[2]
      error(values[2])
    end
    if err then
      errors[#errors + 1] = vim.inspect(err)
    end
    after(result, context, values[2], values[3])
    return unpack(values, 2)
  end
end

local function query(client, buffer, method, params)
  local response, failure = client:request_sync(method, params, 10000, buffer)
  healthy()
  check(response ~= nil, method .. ' timed out: ' .. tostring(failure))
  check(response.err == nil, method .. ' failed: ' .. vim.inspect(response.err))
  record('response', { method = method, result = response.result })
  return response.result
end

local function document(buffer)
  return { textDocument = { uri = vim.uri_from_bufnr(buffer) } }
end

local function native_response(client, buffer, method, params)
  local result, finished, surface
  local handler = observe(method, function(value, _, first, second)
    result, surface, finished = value, { first, second }, true
  end)
  check(client:request(method, params, handler, buffer), 'could not send ' .. method)
  wait_for(method .. ' native handler', function() return finished end)
  record('native-response', { method = method, result = result })
  return result, surface
end

local function set_body(buffer, text)
  api.nvim_set_current_buf(buffer)
  api.nvim_buf_set_lines(buffer, 8, 9, false, { text })
end

local function complete_middle(client, buffer, source, prefix, expected, replacement)
  set_body(buffer, source)
  -- Byte cursor coordinates are editor coordinates. Native request parameter
  -- construction and edit application both use negotiated UTF-16 coordinates.
  api.nvim_win_set_cursor(0, { 9, #prefix })
  check(client.offset_encoding == 'utf-16', 'expected negotiated UTF-16 position encoding')
  local params = vim.lsp.util.make_position_params(0, client.offset_encoding)
  local response = query(client, buffer, 'textDocument/completion', params)
  local candidate
  for _, item in ipairs(response.items or response) do
    if item.textEdit and (item.textEdit.newText == replacement or item.textEdit.newText == 'Dep.' .. replacement) then
      candidate = item
      break
    end
  end
  check(candidate ~= nil, 'missing completion edit for ' .. replacement .. ': ' .. vim.inspect(response))
  check(candidate.textEdit.range ~= nil, 'expected ordinary completion textEdit range')
  -- Neovim's built-in popup uses complete()'s prefix-to-cursor replacement and
  -- ignores a primary textEdit's end range. Exercise the documented native edit
  -- API instead; do not turn that editor limitation into a server expectation.
  vim.lsp.util.apply_text_edits({ candidate.textEdit }, buffer, client.offset_encoding)
  if candidate.additionalTextEdits then
    vim.lsp.util.apply_text_edits(candidate.additionalTextEdits, buffer, client.offset_encoding)
  end
  check(api.nvim_buf_get_lines(buffer, 8, 9, false)[1] == expected,
    'native completion edit application produced: ' .. api.nvim_buf_get_lines(buffer, 8, 9, false)[1])
  check(api.nvim_buf_get_lines(buffer, 7, 8, false)[1] == 'def main() -> U32:', 'completion changed neighboring declaration')
  record('completion-applied', { source = source, prefix = prefix, actual = expected })
end

local function scenario(name, explicit)
  record('scenario-start', { name = name })
  local capabilities = vim.lsp.protocol.make_client_capabilities()
  if explicit then
    capabilities.workspace.didChangeWatchedFiles.dynamicRegistration = true
    capabilities.workspace.didChangeWatchedFiles.relativePatternSupport = true
    capabilities.textDocument.typeHierarchy = { dynamicRegistration = true }
  end
  local watcher_expected = capabilities.workspace.didChangeWatchedFiles.dynamicRegistration == true
  local hierarchy_expected = capabilities.textDocument.typeHierarchy ~= nil
    and capabilities.textDocument.typeHierarchy.dynamicRegistration == true
  record('client-capabilities', { name = name, capabilities = capabilities })
  local registrations, diagnostics = {}, {}
  local attached, initialized, exited, stopping
  local main = vim.fn.bufadd(workspace .. '/main.bend')
  vim.fn.bufload(main)
  api.nvim_set_current_buf(main)
  vim.bo[main].filetype = 'bend'
  -- Open a valid root before the unsaved dependency joins the real client.
  api.nvim_buf_set_lines(main, 8, 9, false, { '  0' })
  local id = vim.lsp.start({
    name = 'bend2-e2e-' .. name,
    cmd = { assert(vim.env.E2E_BINARY) },
    cmd_cwd = workspace,
    root_dir = workspace,
    detached = false,
    capabilities = capabilities,
    flags = { debounce_text_changes = 0 },
    handlers = {
      ['client/registerCapability'] = observe('client/registerCapability', function(result)
        for _, registration in ipairs(result.registrations) do
          registrations[registration.method] = registration
        end
        record('native-registration', result)
      end),
      ['textDocument/publishDiagnostics'] = observe('textDocument/publishDiagnostics', function(result)
        diagnostics[result.uri] = result
        record('native-diagnostics', result)
      end),
    },
    on_init = function(client, result)
      initialized = result
      record('initialize', { name = name, result = result })
      check(result.serverInfo and result.serverInfo.name == 'bend2-lsp', 'missing native serverInfo name')
      check(type(result.serverInfo.version) == 'string' and result.serverInfo.version:match('^%d+%.%d+%.%d+'), 'missing serverInfo version')
      check(client.server_info.name == result.serverInfo.name, 'native client did not retain serverInfo')
      check(result.capabilities.textDocumentSync.change == 2, 'incremental sync was not negotiated')
    end,
    on_attach = function(_, buffer)
      if buffer == main then attached = true end
    end,
    on_error = function(code, message)
      errors[#errors + 1] = tostring(code) .. ': ' .. vim.inspect(message)
    end,
    on_exit = function(code, signal)
      exited = { code = code, signal = signal }
      record('server-exit', { name = name, code = code, signal = signal, stopping = stopping })
      if not stopping or code ~= 0 or signal ~= 0 then
        errors[#errors + 1] = 'unexpected server exit: ' .. vim.inspect(exited)
      end
    end,
  }, { bufnr = main })
  check(id ~= nil, 'native editor failed to launch server')
  local client = assert(vim.lsp.get_client_by_id(id))
  clients[#clients + 1] = client
  wait_for('initialize and main buffer attach', function() return initialized and attached end)
  local dependency = vim.fn.bufadd(workspace .. '/dep.bend')
  vim.fn.bufload(dependency)
  vim.bo[dependency].filetype = 'bend'
  api.nvim_buf_set_lines(dependency, 0, -1, false, { 'def clamp(value: U32) -> U32:', '  value' })
  check(vim.lsp.buf_attach_client(dependency, id), 'unsaved dependency did not attach')
  set_body(main, '  Dep.clamp(1)')
  wait_for('native dynamic registrations', function()
    return (not watcher_expected or registrations['workspace/didChangeWatchedFiles'] ~= nil)
      and (not hierarchy_expected or registrations['textDocument/prepareTypeHierarchy'] ~= nil)
  end)
  local main_uri = vim.uri_from_bufnr(main)
  -- A real request is also a document synchronization barrier, not a sleep.
  local links = query(client, main, 'textDocument/documentLink', document(main))
  check(#links == 2, 'expected meaningful Base and local import links: ' .. vim.inspect(links))
  local base_target, local_target
  for _, link in ipairs(links) do
    if link.range.start.line == 0 then base_target = link.target end
    if link.range.start.line == 1 then local_target = link.target end
  end
  check(local_target == vim.uri_from_bufnr(dependency), 'local documentLink resolves to wrong source')
  check(type(base_target) == 'string' and base_target:match('^file:'), 'Base documentLink has no readable file target')
  -- Open links through native buffer/file APIs and inspect actual target content.
  for _, target in ipairs({ base_target, local_target }) do
    local linked = vim.uri_to_bufnr(target)
    vim.fn.bufload(linked)
    local text = table.concat(api.nvim_buf_get_lines(linked, 0, -1, false), '\n')
    check(text:find(target == base_target and 'def builtin(value: U32)' or 'def clamp(value: U32)', 1, true), 'documentLink target has wrong content: ' .. target)
  end
  check((registrations['workspace/didChangeWatchedFiles'] ~= nil) == watcher_expected, 'watcher registration did not respect native client capabilities')
  check((registrations['textDocument/prepareTypeHierarchy'] ~= nil) == hierarchy_expected, 'type hierarchy registration did not respect native client capabilities')
  if watcher_expected then
    local watchers = registrations['workspace/didChangeWatchedFiles'].registerOptions.watchers
    check(watchers and watchers[1].globPattern == '**/*.bend', 'missing Bend file watcher filter')
  end
  if hierarchy_expected then
    check(client:supports_method('textDocument/prepareTypeHierarchy', main), 'native handler did not install dynamic type hierarchy')
    local shapes = query(client, main, 'textDocument/prepareTypeHierarchy', {
      textDocument = { uri = main_uri }, position = { line = 2, character = 6 },
    })
    check(shapes and shapes[1] and shapes[1].name == 'Shape', 'missing semantic type hierarchy root')
    local children = query(client, main, 'typeHierarchy/subtypes', { item = shapes[1] })
    local names = {}
    for _, child in ipairs(children or {}) do names[child.name] = true end
    check(names.Circle and names.Square, 'missing real type hierarchy constructors')
  end
  api.nvim_set_current_buf(main)
  api.nvim_win_set_cursor(0, { 9, 7 })
  local hover, surface = native_response(client, main, 'textDocument/hover', {
    textDocument = { uri = main_uri }, position = { line = 8, character = 8 },
  })
  check(hover and hover.contents.value:find('def clamp(value: U32) -> U32', 1, true), 'hover did not resolve unsaved dependency signature')
  check(surface[1] and api.nvim_buf_is_valid(surface[1]), 'native hover did not render a preview buffer')
  check(table.concat(api.nvim_buf_get_lines(surface[1], 0, -1, false), '\n'):find('clamp', 1, true), 'native hover preview lost signature content')
  if surface[2] and api.nvim_win_is_valid(surface[2]) then api.nvim_win_close(surface[2], true) end
  local symbols = native_response(client, main, 'textDocument/documentSymbol', document(main))
  local symbol_names = {}
  for _, symbol in ipairs(symbols or {}) do symbol_names[symbol.name] = symbol end
  check(symbol_names.main and symbol_names.local_helper and symbol_names.Shape, 'document symbols lost real declarations')
  check(symbol_names.main.selectionRange.start.line == 7, 'document symbol location is incorrect')
  local main_navigation
  for _, item in ipairs(vim.fn.getqflist()) do
    if item.bufnr == main and item.lnum == 8 and item.text:find('main', 1, true) then
      main_navigation = true
    end
  end
  check(main_navigation, 'native documentSymbol handler did not create navigation to main')
  vim.cmd('cclose')
  api.nvim_set_current_buf(main)
  complete_middle(client, main, '  "😀" + Dep.clampTAIL(1)', '  "😀" + Dep.cl', '  "😀" + Dep.clamp(1)', 'clamp')
  complete_middle(client, main, '  "😀" + loTAIL(1)', '  "😀" + lo', '  "😀" + local_helper(1)', 'local_helper')
  set_body(main, '  missing_first')
  wait_for('compiler error delivered through native diagnostics', function()
    local items = vim.diagnostic.get(main)
    return #items == 1 and items[1].message:find('missing_first', 1, true) ~= nil
  end)
  local items = vim.diagnostic.get(main)
  check(items[1].severity == vim.diagnostic.severity.ERROR, 'compiler diagnostic severity is not an error')
  check(items[1].lnum == 8 and items[1].col == 2 and items[1].end_col == 15, 'native diagnostic caret range is incorrect: ' .. vim.inspect(items))
  check(items[1].message:find('Context:', 1, true), 'compiler diagnostic lost actual error context')
  set_body(main, '  Dep.clamp(1)')
  wait_for('native diagnostics clear after edit', function()
    local latest = diagnostics[main_uri]
    return latest and #latest.diagnostics == 0 and #vim.diagnostic.get(main) == 0
  end)
  stopping = true
  client:stop(false)
  wait_for('graceful shutdown and zero server exit', function() return exited ~= nil end)
  check(exited.code == 0 and exited.signal == 0, 'server did not shut down gracefully')
  for _, buffer in ipairs({ main, dependency }) do
    if api.nvim_buf_is_valid(buffer) then api.nvim_buf_delete(buffer, { force = true }) end
  end
  completed[#completed + 1] = name
  record('scenario-passed', { name = name })
end

local function run()
  vim.lsp.set_log_level('debug')
  -- Let the launcher preserve the native log even if an editor timeout/crash
  -- prevents this script reaching its normal exit path.
  local log_path = assert(io.open(logs .. '/native-log-path.txt', 'w'))
  log_path:write(vim.lsp.log.get_filename())
  log_path:close()
  local event_log = assert(io.open(logs .. '/events.jsonl', 'w'))
  event_log:close()
  scenario('default', false)
  scenario('explicit-dynamic', true)
  healthy()
  local messages = vim.fn.execute('messages')
  check(not messages:match('Error executing') and not messages:match('E%d+:'), 'native editor error messages: ' .. messages)
end

local success, failure = xpcall(run, debug.traceback)
if not success then
  record('failure', { message = tostring(failure) })
  for _, client in ipairs(clients) do
    if not client:is_stopped() then client:stop(true) end
  end
end
local native_log = vim.lsp.log.get_filename()
local source = io.open(native_log, 'r')
if source then
  local target = assert(io.open(logs .. '/lsp.log', 'w'))
  target:write(source:read('*a'))
  source:close()
  target:close()
end
local result = assert(io.open(logs .. '/result.json', 'w'))
result:write(vim.json.encode({ status = success and 'passed' or 'failed', scenarios = completed, error = failure }))
result:close()
if not success then
  io.stderr:write(tostring(failure), '\n')
  vim.cmd('cquit 1')
else
  vim.cmd('qa!')
end
