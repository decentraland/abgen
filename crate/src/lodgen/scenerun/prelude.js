globalThis.global = globalThis;
// module/exports stay off globalThis: cjs_wrap passes this handle as wrapper
// parameters, so sandboxed UMD sniffs see typeof module === 'undefined' and
// pick their AMD branch, matching the Node manifest-builder's wrapper locals
globalThis.__abgen_module = { exports: {} };

globalThis.console = {
  log: () => {},
  info: () => {},
  debug: () => {},
  trace: () => {},
  warn: () => {},
  warning: () => {},
  error: () => {}
};

globalThis.fetch = async (_url, _init) => ({
  status: 200,
  json: async () => undefined,
  text: async () => ''
});

globalThis.WebSocket = class WebSocket {
  constructor(url) {
    this.url = url;
  }
  onmessage() {}
  send() {}
  onclose() {}
  onerror() {}
  onopen() {}
  close(_code, _reason) {}
  readyState = 0;
  CLOSED = 1;
  CLOSING = 0;
  CONNECTING = 0;
  OPEN = 0;
};

const __immediates = [];
globalThis.setImmediate = (fn) => {
  __immediates.push(fn);
};

const __realDate = Date;
let __virtualNowMs = 1704067200000;
globalThis.__advanceClock = (ms) => {
  __virtualNowMs += ms;
};
globalThis.Date = class Date extends __realDate {
  constructor(...args) {
    if (args.length === 0) {
      super(Math.floor(__virtualNowMs));
    } else {
      super(...args);
    }
  }
  static now() {
    return Math.floor(__virtualNowMs);
  }
};

const __modules = {
  AdaptationLayerHelper: {
    getTextureSize: async () => ({})
  },
  EnvironmentApi: {
    isPreviewMode: async () => ({ isPreview: false }),
    getBootstrapData: async () => ({
      id: 'string',
      baseUrl: 'string',
      entity: undefined,
      useFPSThrottling: false
    }),
    getPlatform: async () => ({ platform: 'LOD-generator' }),
    areUnsafeRequestAllowed: async () => ({ status: false }),
    getCurrentRealm: async () => ({}),
    getExplorerConfiguration: async () => ({
      clientUri: '',
      configurations: {
        questsServerUrl: 'https://quests-api.decentraland.org'
      }
    }),
    getDecentralandTime: async () => ({ seconds: Date.now() / 1000 })
  },
  CommsApi: {
    VideoTrackSourceType: {},
    getActiveVideoStreams: async (_) => ({ streams: [] })
  },
  EthereumController: {
    requirePayment: async () => ({ jsonAnyResponse: '' }),
    signMessage: async () => ({ message: '', hexEncodedMessage: '', signature: '' }),
    convertMessageToObject: async () => ({ dict: {} }),
    sendAsync: async () => ({ jsonAnyResponse: '' }),
    getUserAccount: async () => ({})
  },
  EngineApi: {
    sendBatch: async () => ({ events: [] }),
    subscribe: async () => ({ events: [] }),
    unsubscribe: async () => ({ events: [] }),
    crdtGetState: async () => ({
      hasEntities: __abgen.hasEntities,
      data: __abgen.getStateParts()
    }),
    crdtGetMessageFromRenderer: async () => ({ data: [] }),
    crdtSendToRenderer: async ({ data }) => {
      __abgen.sendToRenderer(data instanceof Uint8Array ? data : new Uint8Array(data));
      return { data: [] };
    },
    isServer: async () => ({ isServer: true }),
    ECS6ComponentAttachToAvatar_AttachToAvatarAnchorPointId: {},
    ECS6ComponentCameraModeArea_CameraMode: {},
    ECS6ComponentNftShape_PictureFrameStyle: {},
    ECS6ComponentUiContainerStack_UIStackOrientation: {},
    ECS6ComponentVideoTexture_VideoStatus: {},
    EventDataType: {},
    UiValue_UiValueType: {}
  },
  UserIdentity: {
    getUserData: async () => ({
      data: {
        displayName: 'empty',
        publicKey: 'empty',
        hasConnectedWeb3: true,
        userId: 'empty',
        version: 0,
        avatar: {
          wearables: [''],
          bodyShape: '',
          skinColor: '',
          hairColor: '',
          eyeColor: '',
          snapshots: { face256: '', body: '' }
        }
      }
    }),
    getUserPublicKey: async () => ({})
  },
  SignedFetch: {
    signedFetch: async () => ({ ok: false, status: 404, statusText: 'invalid lod server', headers: {}, body: '' }),
    getHeaders: async () => ({ headers: {} })
  },
  Runtime: {
    getWorldTime: async () => ({ seconds: Date.now() / 1000 }),
    getExplorerInformation: async () => ({ agent: 'lod-server', platform: 'lod-server-platform', configurations: {} }),
    getRealm: async () => ({ realmInfo: undefined }),
    readFile: async ({ fileName }) => __abgen.readFile(fileName),
    getSceneInformation: async () => ({
      urn: 'https://none',
      baseUrl: 'https://none',
      content: [],
      metadataJson: JSON.stringify({
        display: {
          title: '',
          favicon: ''
        },
        owner: '',
        contact: {
          name: '',
          email: ''
        },
        main: 'bin/game.js',
        tags: [],
        scene: {
          parcels: ['-,-'],
          base: '-,-'
        }
      })
    })
  },
  RestrictedActions: {
    triggerEmote: async () => ({}),
    movePlayerTo: async () => ({}),
    changeRealm: async () => ({ success: true }),
    openExternalUrl: async () => ({ success: true }),
    openNftDialog: async () => ({ success: true }),
    setCommunicationsAdapter: async () => ({ success: true }),
    teleportTo: async () => ({}),
    triggerSceneEmote: async () => ({ success: true })
  },
  CommunicationsController: {
    send: async () => ({}),
    sendBinary: async () => ({ data: [] })
  },
  PortableExperiences: {
    exit: async () => ({ status: true }),
    getPortableExperiencesLoaded: async () => ({ loaded: [] }),
    kill: async () => ({ status: true }),
    spawn: async () => ({ name: 'casla', parentCid: '', pid: '' })
  },
  UserActionModule: {
    requestTeleport: async () => ({})
  },
  Players: {
    getPlayerData: async () => ({}),
    getConnectedPlayers: async () => ({ players: [] }),
    getPlayersInScene: async () => ({ players: [] })
  },
  Scene: {
    getSceneInfo: async () => ({ cid: '', metadata: '{}', baseUrl: '', contents: [] })
  }
};

const __loadedModules = {};
globalThis.require = (moduleName) => {
  if (moduleName in __loadedModules) return __loadedModules[moduleName];
  const key = String(moduleName).replace(/^~system\//, '');
  if (key in __modules) {
    __loadedModules[moduleName] = __modules[key];
    return __modules[key];
  }
  throw new Error('Unknown module ' + moduleName);
};

globalThis.__tick = async (kind, dt) => {
  try {
    const exp = globalThis.__abgen_module.exports;
    if (kind === 'start') {
      if (exp.onStart) await exp.onStart();
    } else if (exp.onUpdate) {
      await exp.onUpdate(dt);
    }
  } catch (err) {
    __abgen.log('[' + (kind === 'start' ? 'Start' : 'Update') + ' failed]: ' + err);
  }
  while (__immediates.length) {
    const pending = __immediates.splice(0, __immediates.length);
    for (const fn of pending) {
      try {
        await fn();
      } catch (err) {
        __abgen.log('[setImmediate failed]: ' + err);
      }
    }
  }
};
