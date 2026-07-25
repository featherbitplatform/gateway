-- response-timer.lua
-- Adds an X-Response-Time header showing how long the request took.
-- Place this node BEFORE the upstream to capture the start time,
-- then use a second instance AFTER the upstream to compute the duration.
--
-- Usage: two script nodes in the graph:
--   1. timer-start (this script, phase: before upstream)
--   2. timer-end   (this script, phase: after upstream)

function execute(ctx)
    if not ctx.message._timer_start then
        -- First pass: record start time
        ctx.message._timer_start = os.clock()
    else
        -- Second pass: compute duration and add header
        local elapsed = os.clock() - ctx.message._timer_start
        local ms = string.format("%.2fms", elapsed * 1000)
        ctx.response.headers["x-response-time"] = { ms }
        ctx.message._timer_start = nil
    end

    return ctx
end
