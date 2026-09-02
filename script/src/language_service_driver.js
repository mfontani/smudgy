// Persistent in-memory TypeScript LanguageService used by Smudgy's authoring UI.
// The Rust host owns every file snapshot. This driver never reads the filesystem or
// reaches the network; TypeScript's synchronous host sees only the tables below.
(function () {
  const ts = globalThis.ts;
  if (!ts || typeof ts.createLanguageService !== "function") {
    throw new Error("embedded TypeScript LanguageService is unavailable");
  }

  let libraries = Object.freeze({});
  let libraryRoots = Object.freeze([]);
  let documents = Object.freeze({});
  let roots = Object.freeze([]);
  let projectVersion = 0;
  let initialized = false;
  let poisoned = false;
  let disposed = false;

  const compilerOptions = Object.freeze({
    strict: true,
    noEmit: true,
    target: ts.ScriptTarget.ESNext,
    module: ts.ModuleKind.ESNext,
    moduleResolution: ts.ModuleResolutionKind.Bundler,
    // Keep each authored body lexically isolated. Inline automations execute in
    // their own host-created scope and may legally shadow API globals such as `id`.
    moduleDetection: ts.ModuleDetectionKind.Force,
    allowJs: true,
    checkJs: true,
    allowImportingTsExtensions: true,
    resolveJsonModule: true,
    skipLibCheck: true,
    jsx: ts.JsxEmit.ReactJSX,
    jsxImportSource: "smudgy:widgets",
  });

  function normalize(fileName) {
    let value = String(fileName).replace(/\\/g, "/");
    if (!value.startsWith("/")) value = "/" + value;
    const parts = [];
    for (const part of value.split("/")) {
      if (!part || part === ".") continue;
      if (part === "..") parts.pop();
      else parts.push(part);
    }
    return "/" + parts.join("/");
  }

  function entry(fileName) {
    const name = normalize(fileName);
    return documents[name] || libraries[name];
  }

  function directoryOf(fileName) {
    const name = normalize(fileName);
    const slash = name.lastIndexOf("/");
    return slash <= 0 ? "/" : name.slice(0, slash);
  }

  function extensionFor(fileName) {
    const lower = fileName.toLowerCase();
    if (lower.endsWith(".d.mts")) return ts.Extension.Dmts;
    if (lower.endsWith(".d.cts")) return ts.Extension.Dcts;
    if (lower.endsWith(".d.ts")) return ts.Extension.Dts;
    if (lower.endsWith(".mts")) return ts.Extension.Mts;
    if (lower.endsWith(".cts")) return ts.Extension.Cts;
    if (lower.endsWith(".mjs")) return ts.Extension.Mjs;
    if (lower.endsWith(".cjs")) return ts.Extension.Cjs;
    if (lower.endsWith(".tsx")) return ts.Extension.Tsx;
    if (lower.endsWith(".jsx")) return ts.Extension.Jsx;
    if (lower.endsWith(".js")) return ts.Extension.Js;
    if (lower.endsWith(".json")) return ts.Extension.Json;
    return ts.Extension.Ts;
  }

  function exactRelativeTarget(moduleName, containingFile) {
    if (!moduleName.startsWith(".")) return undefined;
    const containingName = normalize(containingFile);
    const projectMatch = /^\/projects\/[^/]+\/[^/]+\//.exec(containingName);
    const target = normalize(directoryOf(containingFile) + "/" + moduleName);
    if (projectMatch && !target.startsWith(projectMatch[0])) return undefined;
    return entry(target) ? target : undefined;
  }

  const host = {
    getCompilationSettings() {
      return compilerOptions;
    },
    getScriptFileNames() {
      return roots.slice();
    },
    getScriptVersion(fileName) {
      const found = entry(fileName);
      return found ? String(found.version) : "0";
    },
    getScriptSnapshot(fileName) {
      const found = entry(fileName);
      return found ? ts.ScriptSnapshot.fromString(found.text) : undefined;
    },
    getProjectVersion() {
      return String(projectVersion);
    },
    getCurrentDirectory() {
      return "/";
    },
    getDefaultLibFileName() {
      return "/lib.d.ts";
    },
    useCaseSensitiveFileNames() {
      return true;
    },
    getNewLine() {
      return "\n";
    },
    fileExists(fileName) {
      return entry(fileName) !== undefined;
    },
    readFile(fileName) {
      const found = entry(fileName);
      return found && found.text;
    },
    directoryExists(directoryName) {
      const prefix = normalize(directoryName).replace(/\/$/, "") + "/";
      return Object.keys(documents).some((name) => name.startsWith(prefix)) ||
        Object.keys(libraries).some((name) => name.startsWith(prefix));
    },
    getDirectories(directoryName) {
      const prefix = normalize(directoryName).replace(/\/$/, "") + "/";
      const children = new Set();
      for (const name of Object.keys(documents).concat(Object.keys(libraries))) {
        if (!name.startsWith(prefix)) continue;
        const rest = name.slice(prefix.length);
        const slash = rest.indexOf("/");
        if (slash >= 0) children.add(prefix + rest.slice(0, slash));
      }
      return Array.from(children);
    },
    realpath(fileName) {
      return normalize(fileName);
    },
    resolveModuleNames(moduleNames, containingFile) {
      return moduleNames.map((moduleName) => {
        const fileName = exactRelativeTarget(moduleName, containingFile);
        return fileName ? {
          resolvedFileName: fileName,
          extension: extensionFor(fileName),
          isExternalLibraryImport: false,
        } : undefined;
      });
    },
    resolveTypeReferenceDirectives(typeDirectiveNames) {
      return typeDirectiveNames.map((directive) => {
        const name = typeof directive === "string" ? directive : directive.fileName;
        if (name !== "node") return undefined;
        const fileName = "/node-types/@types/node/ts5.6/index.d.ts";
        return entry(fileName) ? {
          resolvedFileName: fileName,
          primary: true,
        } : undefined;
      });
    },
  };

  const service = ts.createLanguageService(host, ts.createDocumentRegistry());

  function makeTable(files, previous) {
    const next = {};
    for (const [rawName, rawText] of Object.entries(files || {})) {
      const name = normalize(rawName);
      const text = String(rawText);
      const old = previous && previous[name];
      next[name] = Object.freeze({
        text,
        version: old && old.text === text ? old.version : old ? old.version + 1 : 1,
      });
    }
    return Object.freeze(next);
  }

  function lineTerminatorLength(text, index) {
    const code = text.charCodeAt(index);
    if (code === 13) {
      return text.charCodeAt(index + 1) === 10 ? 2 : 1;
    }
    return code === 10 || code === 0x2028 || code === 0x2029 ? 1 : 0;
  }

  function offsetAt(fileName, position) {
    const found = entry(fileName);
    if (!found) throw new Error("unknown file: " + fileName);
    const requestedLine = Number(position.line);
    if (!Number.isInteger(requestedLine) || requestedLine < 0) {
      throw new Error("invalid UTF-16 line");
    }
    let start = 0;
    for (let line = 0; line < requestedLine; line += 1) {
      let terminator = start;
      while (terminator < found.text.length && lineTerminatorLength(found.text, terminator) === 0) {
        terminator += 1;
      }
      if (terminator >= found.text.length) throw new Error("line outside file");
      start = terminator + lineTerminatorLength(found.text, terminator);
    }
    let end = start;
    while (end < found.text.length && lineTerminatorLength(found.text, end) === 0) {
      end += 1;
    }
    const character = Number(position.character);
    if (!Number.isInteger(character) || character < 0 || character > end - start) {
      throw new Error("UTF-16 character outside line");
    }
    const offset = start + character;
    if (offset > start && offset < end) {
      const previous = found.text.charCodeAt(offset - 1);
      const current = found.text.charCodeAt(offset);
      if (previous >= 0xd800 && previous <= 0xdbff && current >= 0xdc00 && current <= 0xdfff) {
        throw new Error("UTF-16 character splits a surrogate pair");
      }
    }
    return offset;
  }

  function positionAt(fileName, offset) {
    const found = entry(fileName);
    if (!found) return undefined;
    const bounded = Math.max(0, Math.min(offset, found.text.length));
    let line = 0;
    let lineStart = 0;
    for (let index = 0; index < bounded;) {
      const terminatorLength = lineTerminatorLength(found.text, index);
      if (terminatorLength === 0) {
        index += 1;
        continue;
      }
      if (bounded < index + terminatorLength) {
        throw new Error("offset splits a CRLF line terminator");
      }
      line += 1;
      index += terminatorLength;
      lineStart = index;
    }
    return { line, character: bounded - lineStart };
  }

  function range(fileName, textSpan) {
    if (!textSpan || !entry(fileName)) return undefined;
    return {
      start: positionAt(fileName, textSpan.start),
      end: positionAt(fileName, textSpan.start + textSpan.length),
    };
  }

  function display(parts) {
    return ts.displayPartsToString(parts || []);
  }

  function diagnostic(value) {
    const fileName = value.file && normalize(value.file.fileName);
    return {
      fileName,
      code: value.code,
      category: ts.DiagnosticCategory[value.category],
      message: ts.flattenDiagnosticMessageText(value.messageText, "\n"),
      range: fileName && value.start !== undefined && value.length !== undefined
        ? range(fileName, { start: value.start, length: value.length })
        : undefined,
    };
  }

  function handle(request) {
    const method = request.method;
    const params = request.params || {};
    if (disposed) throw new Error("LanguageService is disposed");
    if (method === "initialize") {
      if (initialized) throw new Error("LanguageService is already initialized");
      libraries = makeTable(params.libraries || {}, undefined);
      // TypeScript's triple-slash `lib` parser consults an internal name map before
      // asking the compiler host for a file. Register only Smudgy's embedded Deno libs;
      // absent host libs such as DOM remain unresolvable and cannot widen the project.
      if (ts.libMap && typeof ts.libMap.set === "function") {
        for (const fileName of Object.keys(libraries)) {
          const match = /^\/lib\.(deno(?:[._][^.]+)*)\.d\.ts$/i.exec(fileName);
          if (match) ts.libMap.set(match[1].toLocaleLowerCase(), fileName.slice(1));
        }
      }
      libraryRoots = Object.freeze(Array.from(new Set(
        (params.libraryRoots || []).map(normalize),
      )).sort());
      for (const root of libraryRoots) {
        if (!libraries[root]) throw new Error("unknown library root: " + root);
      }
      roots = libraryRoots;
      initialized = true;
      projectVersion += 1;
      return { typescriptVersion: ts.version };
    }
    if (!initialized) throw new Error("LanguageService is not initialized");
    if (method === "dispose") {
      service.dispose();
      libraries = Object.freeze({});
      libraryRoots = Object.freeze([]);
      documents = Object.freeze({});
      roots = Object.freeze([]);
      initialized = false;
      disposed = true;
      return { disposed: true };
    }
    if (poisoned) throw new Error("LanguageService snapshot is poisoned");
    if (method === "replaceProject") {
      const previousDocuments = documents;
      const previousRoots = roots;
      const previousProjectVersion = projectVersion;
      try {
        documents = makeTable(params.files || {}, documents);
        roots = Object.freeze(Array.from(new Set(
          libraryRoots.concat(Object.keys(documents)),
        )).sort());
        projectVersion += 1;
        // Force TypeScript to observe the new immutable snapshot before acknowledging it.
        service.getProgram();
      } catch (error) {
        documents = previousDocuments;
        roots = previousRoots;
        projectVersion = previousProjectVersion;
        try {
          service.getProgram();
        } catch (_) {
          poisoned = true;
          throw new Error("LanguageService snapshot rollback failed");
        }
        throw error;
      }
      return { projectVersion, roots: roots.slice() };
    }
    if (method === "diagnostics") {
      const fileName = normalize(params.fileName);
      const values = service.getSyntacticDiagnostics(fileName)
        .concat(service.getSemanticDiagnostics(fileName))
        .concat(service.getSuggestionDiagnostics(fileName));
      return { diagnostics: values.map(diagnostic) };
    }
    if (method === "completion") {
      const fileName = normalize(params.fileName);
      const position = offsetAt(fileName, params.position);
      const info = service.getCompletionsAtPosition(fileName, position, {
        includeCompletionsForModuleExports: false,
        includeCompletionsWithInsertText: true,
      });
      // This protocol cannot yet apply completion code actions. Never surface an
      // auto-import entry whose insertion would therefore leave invalid source.
      // Explicit import-clause and member completions carry neither marker.
      const sourceEntries = (info && info.entries || []).filter(
        (value) => !value.source && !value.hasAction,
      );
      // TypeScript returns a broad candidate set and expects the client to
      // filter it against the replacement-span prefix. The Automations UI is
      // intentionally small, so filter before its bounded visible list instead
      // of allowing alphabetically early keywords to hide the requested name.
      const sourceText = entry(fileName).text;
      const sharedSpan = info && info.optionalReplacementSpan;
      const matchingEntries = sourceEntries.filter((value) => {
        const span = value.replacementSpan || sharedSpan;
        if (!span || span.start > position) return true;
        const prefix = sourceText.slice(span.start, position).toLocaleLowerCase();
        if (!prefix) return true;
        const candidate = String(value.filterText || value.insertText || value.name)
          .toLocaleLowerCase();
        return candidate.startsWith(prefix);
      });
      const entries = matchingEntries.slice(0, 500).map((value) => ({
        name: value.name,
        kind: value.kind,
        sortText: value.sortText,
        insertText: value.insertText,
        replacementSpan: range(
          fileName,
          value.replacementSpan || (info && info.optionalReplacementSpan),
        ),
        deprecated: !!(value.kindModifiers && value.kindModifiers.includes("deprecated")),
      }));
      return {
        isIncomplete: !!(info && info.isIncomplete) || matchingEntries.length > entries.length,
        entries,
      };
    }
    if (method === "hover") {
      const fileName = normalize(params.fileName);
      const position = offsetAt(fileName, params.position);
      const info = service.getQuickInfoAtPosition(fileName, position);
      if (!info) return { hover: null };
      const documentation = display(info.documentation);
      const tags = (info.tags || []).map((tag) => `@${tag.name} ${display(tag.text)}`).join("\n");
      return {
        hover: {
          range: range(fileName, info.textSpan),
          display: display(info.displayParts),
          documentation: [documentation, tags].filter(Boolean).join("\n\n"),
        },
      };
    }
    if (method === "definition") {
      const fileName = normalize(params.fileName);
      const position = offsetAt(fileName, params.position);
      const result = service.getDefinitionAndBoundSpan(fileName, position);
      return {
        definitions: (result && result.definitions || []).map((value) => ({
          fileName: normalize(value.fileName),
          range: range(value.fileName, value.textSpan),
        })),
      };
    }
    if (method === "format") {
      const fileName = normalize(params.fileName);
      const options = params.options || {};
      const edits = service.getFormattingEditsForDocument(fileName, {
        indentSize: Number(options.tabSize) || 2,
        tabSize: Number(options.tabSize) || 2,
        convertTabsToSpaces: options.insertSpaces !== false,
        semicolons: ts.SemicolonPreference.Insert,
        newLineCharacter: "\n",
      });
      return {
        edits: edits.map((edit) => ({
          range: range(fileName, edit.span),
          newText: edit.newText,
        })),
      };
    }
    throw new Error("unknown LanguageService method: " + method);
  }

  globalThis.__SMUDGY_LANGUAGE_SERVICE_HANDLE = function (requestJson) {
    try {
      return JSON.stringify({ ok: true, result: handle(JSON.parse(requestJson)) });
    } catch (error) {
      return JSON.stringify({
        ok: false,
        error: error && error.stack ? String(error.stack) : String(error),
      });
    }
  };
})();
