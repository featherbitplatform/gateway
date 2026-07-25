-- add-request-id.lua
-- Injects a unique X-Request-Id header into the request before it reaches the upstream.

local counter = 0

function execute(ctx)
    counter = counter + 1
    local request_id = string.format("%s-%d", os.clock(), counter)

    -- Add to request headers (upstream will see it)
    ctx.request.headers["x-request-id"] = { request_id }

    -- Store in message so downstream plugins (e.g., logging) can access it
    ctx.message.request_id = request_id

    return ctx
end
