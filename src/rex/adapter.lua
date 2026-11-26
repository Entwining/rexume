-- Connects `rexume daemon` to the server over this script's one connection. `rexume relay` carries
-- bytes between this script and the daemon's socket: the daemon writes one call per line, and
-- each reply and event goes back as one JSON line.
local relay
local unread = ""

local function to_daemon(message)
  local ok, err = relay:write(rex.json.encode(message) .. "\n")
  if not ok then
    io.stderr:write("rexume adapter: the daemon is gone: " .. tostring(err) .. "\n")
    rex.stop(1)
  end
end

-- Without detach, Rex stops the command when the script's top level finishes. The relay still
-- ends with this script: its standard input closes when `rex do` exits.
local err
relay, err = rex.exec.start(rex.args.relay, {
  stdin = true,
  detach = true,
  stdout = function(data)
    unread = unread .. data
    while true do
      local newline = unread:find("\n", 1, true)
      if not newline then return end
      local request = rex.json.decode(unread:sub(1, newline - 1))
      unread = unread:sub(newline + 1)
      local result, failure = rex.call(request.method, request.params)
      to_daemon({ id = request.id, result = result, error = failure })
    end
  end,
  stderr = function(data) io.stderr:write(data) end,
})
if not relay then error(err, 0) end
-- The token is the first line the daemon reads. Nothing else writes to the relay before it: the
-- daemon sends no call until it has read the token, and no event handler is registered yet.
local ok, failure = relay:write(rex.args.token .. "\n")
if not ok then error(failure, 0) end

local function forward(ctx, ev)
  to_daemon({
    event = ev.type == "block_event" and ev.name or ev.type,
    session_id = ctx.session_id or ev.session_id,
    block_id = ctx.block_id or ev.block_id,
    payload = ev.payload,
  })
end
for _, name in ipairs({ "client_joined", "session_created", "session_view_changed", "block_closed",
  "terminal.process_changed", "terminal.program_status_changed", "terminal.program_status_removed",
  "terminal.child_exited", "terminal.program_resume_changed" }) do
  rex.on(name, forward)
end
