local combustivel = tonumber(pulse.telemetry["fuel"]) or 0
local status = pulse.telemetry["status"]

if combustivel < 70 then 
   return {
        action = "SHUTDOWN",
        cmd_name = "EMERGENCY_CUTOFF",
        priority = 1000,
        params = { ["reason"] = "Overheating" }
    }
end