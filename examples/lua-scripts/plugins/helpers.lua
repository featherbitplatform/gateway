-- helpers.lua
-- Shared utility module. Import with: local helpers = require("helpers")

local M = {}

--- Generates a simple request ID from clock + counter
local counter = 0
function M.generate_id()
    counter = counter + 1
    return string.format("%s-%d", os.clock(), counter)
end

--- Checks if a string contains a pattern (case-insensitive)
function M.contains_ci(str, pattern)
    return string.find(string.lower(str), string.lower(pattern)) ~= nil
end

--- Splits a string by delimiter
function M.split(str, delimiter)
    local result = {}
    for part in string.gmatch(str, "([^" .. delimiter .. "]+)") do
        result[#result + 1] = part
    end
    return result
end

return M
