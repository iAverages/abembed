local root = vim.fs.dirname(vim.fs.abspath(debug.getinfo(1, "S").source:sub(2)))
vim.opt.runtimepath:append(root .. "/.nvim")
