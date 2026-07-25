-- with-require-example.lua
-- Demonstrates using require() to import a shared module.
-- The helpers.lua file must be in the same directory as this script.
--
-- Usage in config:
--   source: /etc/gateway/plugins/with-require-example.lua
--
-- Or with explicit modules_path:
--   source: /path/to/script.lua
--   modules_path: /etc/gateway/plugins

local helpers = require("helpers")

function execute(ctx)
    -- Use the shared helper to generate a request ID
    local request_id = helpers.generate_id()
    ctx.request.headers["x-request-id"] = { request_id }
    ctx.message.request_id = request_id

    -- Use the shared helper to check user agent
    local ua_list = ctx.request.headers["user-agent"]
    if ua_list then
        local ua = ua_list[1] or ""
        if helpers.contains_ci(ua, "curl") then
            ctx.message.is_cli_client = true
        end
    end

    return ctx
end
