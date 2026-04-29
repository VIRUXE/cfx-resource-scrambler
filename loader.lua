__SCRIPTS = {
  server = {},
  client = {},
}

-- A no-op that can be called with any number of args and chained
-- (`foo 'bar' 'baz'` is two calls in Lua's call-without-parens form).
function dummy(...) return dummy end

local function append_string_or_table(list, data)
  if type(data) == 'table' then
    for i = 1, #data, 1 do
      table.insert(list, data[i])
    end
  else
    table.insert(list, data)
  end
end

server_script = function(data) append_string_or_table(__SCRIPTS.server, data) end
client_script = function(data) append_string_or_table(__SCRIPTS.client, data) end
shared_script = function(data)
  append_string_or_table(__SCRIPTS.server, data)
  append_string_or_table(__SCRIPTS.client, data)
end

server_scripts = server_script
client_scripts = client_script
shared_scripts = shared_script

-- Any other manifest directive (description, version, name, dependency,
-- author, fx_version, ui_page, file, …) silently becomes a no-op. This is
-- much more robust than maintaining a hand-rolled allow-list as new fxmanifest
-- features ship.
setmetatable(_G, { __index = function(_, _) return dummy end })
