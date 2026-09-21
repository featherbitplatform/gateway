-- block-user-agents.lua
-- Rejects requests from known scraper/bot User-Agent patterns with a 403.
--
-- The script prepares the response and returns it with "respond", so the
-- node leaves on its `respond` port (wired to client) and the upstream never
-- runs. Setting ctx.response alone would not stop the request: the upstream
-- replaces the response. The port is named, never inferred.

local blocked_patterns = {
    "python%-requests",
    "scrapy",
    "wget",
    "go%-http%-client",
}

function execute(ctx)
    local ua_list = ctx.request.headers["user-agent"]
    if not ua_list then
        return ctx
    end

    local ua = ua_list[1] or ""
    local ua_lower = string.lower(ua)

    for _, pattern in ipairs(blocked_patterns) do
        if string.find(ua_lower, pattern) then
            ctx.response.status_code = 403
            ctx.response.body = '{"error": "forbidden", "message": "Blocked user agent"}'
            ctx.response.headers["content-type"] = { "application/json" }
            ctx.message.blocked_ua = ua -- for traces and loggers; nothing branches on it
            return ctx, "respond"
        end
    end

    return ctx
end
