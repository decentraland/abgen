const PLATFORM_PACKAGES = {
  'linux-x64': ['@dcl/abgen-linux-x64', 'abgen'],
  'linux-arm64': ['@dcl/abgen-linux-arm64', 'abgen'],
  'win32-x64': ['@dcl/abgen-win32-x64', 'abgen.exe'],
  'win32-arm64': ['@dcl/abgen-win32-arm64', 'abgen.exe'],
  'darwin-arm64': ['@dcl/abgen-darwin-arm64', 'abgen'],
  'darwin-x64': ['@dcl/abgen-darwin-x64', 'abgen']
}

function binPath() {
  const key = `${process.platform}-${process.arch}`
  const entry = PLATFORM_PACKAGES[key]
  if (!entry) {
    throw new Error(
      `@dcl/abgen: no prebuilt binary for ${key} (available: ${Object.keys(PLATFORM_PACKAGES).join(', ')}). ` +
        'Build abgen from source (https://github.com/decentraland/abgen) and put it on the PATH instead.'
    )
  }
  const [pkg, bin] = entry
  try {
    return require.resolve(`${pkg}/${bin}`)
  } catch {
    throw new Error(
      `@dcl/abgen: ${pkg} is missing. It installs as an optionalDependency of @dcl/abgen - ` +
        'reinstall without --omit=optional / --no-optional, or add it as a direct dependency.'
    )
  }
}

module.exports = { binPath, PLATFORM_PACKAGES }
