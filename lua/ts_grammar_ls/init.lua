local M = {}

local INPUT_SUFFIX = ".tsg-repl.tsg"
local TREE_SUFFIX = ".tsg-repl-tree.tsg"

local function normalize(path)
	return vim.fs.normalize(path)
end

local function is_repl_path(path)
	return path:sub(-#INPUT_SUFFIX) == INPUT_SUFFIX or path:sub(-#TREE_SUFFIX) == TREE_SUFFIX
end

local function is_repl_buffer(buf)
	return vim.api.nvim_buf_is_valid(buf) and is_repl_path(normalize(vim.api.nvim_buf_get_name(buf)))
end

local function configure_repl_buffer(buf)
	if not is_repl_buffer(buf) then
		return
	end

	-- The files give the LSP stable document URIs, but their editor buffers
	-- are scratch state. In particular, they must never block :q or :qa.
	vim.bo[buf].buftype = "nofile"
	vim.bo[buf].bufhidden = "wipe"
	vim.bo[buf].swapfile = false
	vim.bo[buf].buflisted = false
end

local function focus_non_repl_window()
	for _, win in ipairs(vim.api.nvim_list_wins()) do
		if vim.api.nvim_win_is_valid(win) and not is_repl_buffer(vim.api.nvim_win_get_buf(win)) then
			vim.api.nvim_set_current_win(win)
			return
		end
	end
end

local function path_from_uri(uri)
	if not uri then
		return nil
	end

	local ok, path = pcall(vim.uri_to_fname, uri)
	return ok and normalize(path) or nil
end

---Close the grammar REPL buffers and return focus to the grammar.
---@param opts? { grammar_uri?: string, input_uri?: string, tree_uri?: string, client_id?: integer }
function M.close_repl(opts)
	opts = opts or {}

	if opts.grammar_uri then
		local client = opts.client_id and vim.lsp.get_client_by_id(opts.client_id) or nil
		vim.lsp.util.show_document(
			{ uri = opts.grammar_uri },
			client and client.offset_encoding or "utf-16",
			{ reuse_win = true, focus = true }
		)
	else
		-- openRepl preserves the grammar window, so this is sufficient for
		-- the standalone command and avoids depending on server metadata.
		focus_non_repl_window()
	end

	local target_paths = {}
	for _, key in ipairs({ "input_uri", "tree_uri" }) do
		local path = path_from_uri(opts[key])
		if path then
			target_paths[path] = true
		end
	end
	local has_targets = next(target_paths) ~= nil

	local repl_buffers = {}
	for _, buf in ipairs(vim.api.nvim_list_bufs()) do
		if vim.api.nvim_buf_is_valid(buf) then
			local path = normalize(vim.api.nvim_buf_get_name(buf))
			if (has_targets and target_paths[path]) or (not has_targets and is_repl_path(path)) then
				repl_buffers[buf] = true
			end
		end
	end

	-- Deleting a buffer merely replaces it in each window; it does not remove
	-- the split. Close the REPL windows explicitly while the grammar is focused.
	for _, win in ipairs(vim.api.nvim_list_wins()) do
		if vim.api.nvim_win_is_valid(win) and repl_buffers[vim.api.nvim_win_get_buf(win)] then
			pcall(vim.api.nvim_win_close, win, true)
		end
	end

	for buf in pairs(repl_buffers) do
		if vim.api.nvim_buf_is_valid(buf) then
			vim.api.nvim_buf_delete(buf, { force = true })
		end
	end
end

function M.setup()
	vim.filetype.add({ extension = { tsg = "tsg" } })

	local group = vim.api.nvim_create_augroup("ts_grammar_ls_nvim", { clear = true })

	vim.api.nvim_create_autocmd("FileType", {
		group = group,
		pattern = "tsg",
		callback = function(event)
			vim.lsp.codelens.enable(true, { bufnr = event.buf })
		end,
	})

	vim.api.nvim_create_autocmd("LspAttach", {
		group = group,
		callback = function(event)
			local client = vim.lsp.get_client_by_id(event.data.client_id)
			if client and client.name == "ts_grammar_ls" then
				configure_repl_buffer(event.buf)
			end
		end,
	})

	vim.lsp.commands["tsg.returnToGrammar"] = function(command, ctx)
		local args = command.arguments and command.arguments[1]
		if not args then
			return
		end

		M.close_repl(vim.tbl_extend("force", args, { client_id = ctx.client_id }))
	end

	vim.api.nvim_create_user_command("TsgReplClose", function()
		M.close_repl()
	end, {
		desc = "Close grammar REPL buffers and return to the grammar",
		force = true,
	})

	-- Also repair REPL buffers that predate setup(), which is useful when
	-- developing and reloading this module in an existing Neovim session.
	for _, buf in ipairs(vim.api.nvim_list_bufs()) do
		configure_repl_buffer(buf)
	end
end

return M
