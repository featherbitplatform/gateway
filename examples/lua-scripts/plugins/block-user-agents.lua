-- block-user-agents.lua
-- Flags requests from specific User-Agent patterns (scrapers, bots) by
-- setting ctx.message.blocked_ua. The policy branches on it with a
-- `condition` node and answers 403 from a `response-rewrite` node.
--
-- A script cannot reject a request by writing ctx.response before the
-- upstream: the upstream node replaces the response. Branch instead.

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
            ctx.message.blocked_ua = ua
            return ctx
        end
    end

    return ctx
end
