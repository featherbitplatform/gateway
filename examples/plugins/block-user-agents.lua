-- block-user-agents.lua
-- Blocks requests from specific User-Agent patterns (e.g., scrapers, bots).

local blocked_patterns = {
    "curl",
    "python%-requests",
    "scrapy",
    "wget",
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
            ctx.message.blocked_ua = ua
            return ctx
        end
    end

    return ctx
end
