-- Three consecutive hot readings => SHUTDOWN. The counter is shared memory: with Raft it is the
-- same counter on every node, so it keeps counting when the agent fails over to another host.
local temp = tonumber(pulse.telemetry["cpu_temp"]) or 0
local strikes = redb_api.get(pulse.agent_id .. "_strikes") or 0   -- nil when absent

if temp > 90 then
    strikes = redb_api.incr(pulse.agent_id .. "_strikes")          -- atomic, cluster-safe
    if strikes >= 3 then
        redb_api.set(pulse.agent_id .. "_strikes", 0)
        return { action = "SHUTDOWN", cmd_name = "PERSISTENT_OVERHEAT", priority = 1000,
                 params = { msg = "temperature stayed critical", strikes = strikes } }
    end
elseif strikes > 0 then
    redb_api.set(pulse.agent_id .. "_strikes", 0)
end
return nil   -- no opinion
