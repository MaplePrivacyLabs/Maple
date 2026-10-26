-- Record what Neovim does for each composer Vim case.
--
-- Usage (from apps/maple-agent): just vim-cases
--
-- Reads a JSON array of {"text", "keys"} cases, where "ˇ" in `text` marks
-- the cursor, runs each case in a clean Neovim, and rewrites the file with
-- an "expect" object per case. Keys use Vim notation (`<Esc>`, `<C-r>`).
-- The composer test replays the same keys through the real keymap and
-- compares the result with the recording, so the recording is the only
-- place that needs Neovim.
--
-- Neovim runs with its defaults except `formatoptions` (no comment-leader
-- handling when joining), `startofline` (the composer follows Vim, which
-- puts the cursor on the first non-blank after a linewise delete), and the
-- message options that keep the recording quiet.

local path = assert(arg[1], 'usage: nvim --clean --headless -l record-vim-cases.lua CASES.json')
local CURSOR = 'ˇ'

local function read(file)
  local handle = assert(io.open(file, 'r'))
  local content = handle:read('*a')
  handle:close()
  return content
end

local function offset_of(lines, row, col)
  local offset = 0
  for index = 1, row - 1 do
    offset = offset + #lines[index] + 1
  end
  return offset + col
end

local function position_of(text, offset)
  local before = text:sub(1, offset)
  local row = select(2, before:gsub('\n', '')) + 1
  local line_start = before:match('.*\n()') or 1
  return row, offset - line_start + 1
end

local modes = { n = 'normal', i = 'insert', v = 'visual', V = 'visual_line' }

local recorded
_G.__vim_case_record = function()
  local mode = vim.api.nvim_get_mode().mode
  local lines = vim.api.nvim_buf_get_lines(0, 0, -1, false)
  local cursor = vim.api.nvim_win_get_cursor(0)
  local anchor = vim.fn.getpos('v')
  recorded = {
    mode = modes[mode] or error('unexpected Neovim mode ' .. mode),
    text = table.concat(lines, '\n'),
    cursor = offset_of(lines, cursor[1], cursor[2]),
    anchor = offset_of(lines, anchor[2], anchor[3] - 1),
    register = vim.fn.getreg('"'),
    register_linewise = vim.fn.getregtype('"') == 'V',
  }
end

vim.o.showmode = false
vim.o.report = 99999
vim.o.formatoptions = ''
vim.o.startofline = true

local function run(case)
  local marker = case.text:find(CURSOR, 1, true)
  assert(marker, 'case has no cursor marker: ' .. case.text)
  local text = case.text:sub(1, marker - 1) .. case.text:sub(marker + #CURSOR)
  assert(not text:find(CURSOR, 1, true), 'case has two cursor markers: ' .. case.text)

  vim.cmd('enew!')
  vim.api.nvim_buf_set_lines(0, 0, -1, false, vim.split(text, '\n', { plain = true }))
  local row, col = position_of(text, marker - 1)
  vim.api.nvim_win_set_cursor(0, { row, col })
  vim.fn.setreg('"', '', 'v')
  recorded = nil
  _G.__vim_case_keys =
    vim.api.nvim_replace_termcodes(case.keys .. '<Cmd>lua __vim_case_record()<CR>', true, false, true)
  -- `silent` keeps undo and change messages out of the output.
  vim.cmd('silent lua vim.api.nvim_feedkeys(__vim_case_keys, "xt", false)')
  assert(recorded, 'Neovim was still waiting for keys after: ' .. case.keys)
  vim.api.nvim_feedkeys(vim.api.nvim_replace_termcodes('<Esc><Esc>', true, false, true), 'xt', false)

  local expect = recorded
  if expect.mode ~= 'visual' and expect.mode ~= 'visual_line' then
    expect.anchor = nil
  end
  return expect
end

local function encode_case(case, expect)
  local fields = {
    string.format('"text": %s', vim.json.encode(case.text)),
    string.format('"keys": %s', vim.json.encode(case.keys)),
  }
  local result = {
    string.format('"mode": %s', vim.json.encode(expect.mode)),
    string.format('"text": %s', vim.json.encode(expect.text)),
    string.format('"cursor": %d', expect.cursor),
  }
  if expect.anchor then
    result[#result + 1] = string.format('"anchor": %d', expect.anchor)
  end
  result[#result + 1] = string.format('"register": %s', vim.json.encode(expect.register))
  result[#result + 1] = string.format('"register_linewise": %s', tostring(expect.register_linewise))
  fields[#fields + 1] = string.format('"expect": {%s}', table.concat(result, ', '))
  return '  {' .. table.concat(fields, ', ') .. '}'
end

local cases = vim.json.decode(read(path))
local encoded = {}
for index, case in ipairs(cases) do
  local ok, expect = pcall(run, case)
  if not ok then
    io.stderr:write(string.format('case %d (%s): %s\n', index, case.keys, expect))
    os.exit(1)
  end
  encoded[#encoded + 1] = encode_case(case, expect)
end

local handle = assert(io.open(path, 'w'))
handle:write('[\n' .. table.concat(encoded, ',\n') .. '\n]\n')
handle:close()
io.stdout:write(string.format('recorded %d cases with %s\n', #cases, vim.fn.execute('version'):match('NVIM v[^\n]+')))
