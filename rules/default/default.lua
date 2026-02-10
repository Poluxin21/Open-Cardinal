-- This script is executed when Cardinal cannot find specific rules for an Agent.
-- It acts as a fallback safety net for any connected device.

-- The 'pulse' object is automatically injected by the Rust Kernel.
-- Structure: pulse.agent_id (string), pulse.telemetry (table/map)

print("[DEFAULT] Processing generic rules for: " .. pulse.agent_id)

-- 1. Safe Telemetry Reading (with default values)
-- We use 'tonumber' to ensure we are working with numbers, defaulting to 0 if nil.
local cpu = tonumber(pulse.telemetry["cpu_usage"]) or 0
local ram = tonumber(pulse.telemetry["memory_usage"]) or 0
local temp = tonumber(pulse.telemetry["temperature"]) or 0

-- 2. Universal Safety Rule: Overheating Protection
-- If any device reports > 95 degrees, we force a shutdown immediately.
if temp > 95 then
    return {
        action = "SHUTDOWN",           -- Maps to ActionType::SHUTDOWN (1)
        cmd_name = "EMERGENCY_HEAT_STOP",
        priority = 100,                -- High priority ensures this rule overrides others
        params = {
            ["reason"] = "Critical Temperature exceeded 95C",
            ["current_temp"] = tostring(temp)
        }
    }
end

-- 3. Warning Rule: High Load
-- If CPU usage is critical, we send a custom warning to the logging system.
if cpu > 90 then
    return {
        action = "CUSTOM",             -- Maps to ActionType::CUSTOM (3)
        cmd_name = "LOG_WARNING",
        priority = 50,                 -- Medium priority
        params = {
            ["msg"] = "High CPU load detected on default agent",
            ["load"] = tostring(cpu) .. "%"
        }
    }
end

-- 4. Default Behavior (IDLE)
-- If no anomalies are detected, Cardinal acknowledges the heartbeat.
return {
    action = "IDLE",                   -- Maps to ActionType::IDLE (0)
    cmd_name = "HEARTBEAT_ACK",
    priority = 0,                      -- Lowest priority
    params = {}
}