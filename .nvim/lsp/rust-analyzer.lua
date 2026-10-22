return {
	root_dir = function(path, default)
		return vim.fs.root(path, "Cargo.toml") or default(path)
	end,
	before_init = function(_, config)
		local manifest = config.root_dir .. "/Cargo.toml"
		local package = vim.fs.basename(config.root_dir)
		local target = package == "abembed" and "wasm32-unknown-unknown" or vim.NIL

		config.settings = vim.tbl_deep_extend("force", config.settings or {}, {
			["rust-analyzer"] = {
				linkedProjects = { manifest },
				cargo = {
					extraEnv = {
						CARGO_TARGET_DIR = vim.fs.joinpath(
							vim.fn.stdpath("cache"),
							"rust-analyzer",
							"abembed",
							package
						),
					},
					target = target,
				},
			},
		})
	end,
}
