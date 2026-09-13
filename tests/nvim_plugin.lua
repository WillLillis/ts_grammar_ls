local project_root = assert(arg[1], "expected the project root")
vim.opt.runtimepath:prepend(project_root)

local plugin = require("ts_grammar_ls")
plugin.setup()
plugin.setup() -- setup is safe to repeat while iterating on the plugin.

local function make_buffer(path, text)
	local buf = vim.api.nvim_create_buf(true, false)
	vim.api.nvim_buf_set_name(buf, path)
	vim.api.nvim_buf_set_lines(buf, 0, -1, false, { text })
	return buf
end

local grammar = make_buffer("/tmp/grammar.tsg", "rule source_file {}")
local input = make_buffer("/tmp/session.tsg-repl.tsg", "input")
local tree = make_buffer("/tmp/session.tsg-repl-tree.tsg", "(source_file)")

local initial_window_count = #vim.api.nvim_list_wins()

vim.api.nvim_set_current_buf(grammar)
vim.cmd.vsplit()
vim.api.nvim_set_current_buf(input)
vim.cmd.vsplit()
vim.api.nvim_set_current_buf(tree)

local original_get_client = vim.lsp.get_client_by_id
vim.lsp.get_client_by_id = function()
	return { name = "ts_grammar_ls" }
end
vim.api.nvim_exec_autocmds("LspAttach", {
	buffer = input,
	data = { client_id = 1 },
})
vim.api.nvim_exec_autocmds("LspAttach", {
	buffer = tree,
	data = { client_id = 1 },
})
vim.lsp.get_client_by_id = original_get_client

for _, buf in ipairs({ input, tree }) do
	assert(vim.bo[buf].buftype == "nofile")
	assert(vim.bo[buf].bufhidden == "wipe")
	assert(not vim.bo[buf].swapfile)
	assert(not vim.bo[buf].buflisted)
end

vim.cmd.TsgReplClose()
assert(not vim.api.nvim_buf_is_valid(input))
assert(not vim.api.nvim_buf_is_valid(tree))
assert(vim.api.nvim_get_current_buf() == grammar)
assert(#vim.api.nvim_list_wins() == initial_window_count)

local input_from_lens = make_buffer("/tmp/lens.tsg-repl.tsg", "input")
local tree_from_lens = make_buffer("/tmp/lens.tsg-repl-tree.tsg", "(source_file)")
vim.cmd.vsplit()
vim.api.nvim_set_current_buf(input_from_lens)
vim.cmd.vsplit()
vim.api.nvim_set_current_buf(tree_from_lens)

vim.lsp.get_client_by_id = function()
	return { name = "ts_grammar_ls", offset_encoding = "utf-16" }
end
vim.lsp.commands["tsg.returnToGrammar"]({
	command = "tsg.returnToGrammar",
	arguments = {
		{
			grammar_uri = vim.uri_from_fname("/tmp/grammar.tsg"),
			input_uri = vim.uri_from_fname("/tmp/lens.tsg-repl.tsg"),
			tree_uri = vim.uri_from_fname("/tmp/lens.tsg-repl-tree.tsg"),
		},
	},
}, { client_id = 1 })
vim.lsp.get_client_by_id = original_get_client

assert(not vim.api.nvim_buf_is_valid(input_from_lens))
assert(not vim.api.nvim_buf_is_valid(tree_from_lens))
assert(vim.api.nvim_get_current_buf() == grammar)
assert(#vim.api.nvim_list_wins() == initial_window_count)

print("ts_grammar_ls.nvim: ok")
