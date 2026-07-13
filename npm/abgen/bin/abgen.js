#!/usr/bin/env node
const { spawn } = require('child_process')
const { binPath } = require('../index.js')

const child = spawn(binPath(), process.argv.slice(2), { stdio: 'inherit' })
child.on('error', (error) => {
  console.error(`@dcl/abgen: ${error.message}`)
  process.exit(1)
})
for (const signal of ['SIGINT', 'SIGTERM', 'SIGHUP']) {
  process.on(signal, () => child.kill(signal))
}
child.on('exit', (code) => {
  process.exit(code === null ? 1 : code)
})
