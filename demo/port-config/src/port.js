'use strict';

// Parse a TCP port from a config string such as "8080".
function parsePort(value) {
  const port = parseInt(value, 10);
  return port;
}

module.exports = { parsePort };
