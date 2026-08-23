// Managed by smudgy. This is the deliberately narrow online Web Audio surface
// supported by Smudgy's hosted session output. It is not the browser DOM lib.

type AudioContextLatencyCategory = "balanced" | "interactive" | "playback";
type AudioContextState = "closed" | "running" | "suspended";
type OscillatorType = "sawtooth" | "sine" | "square" | "triangle";

interface AudioSinkOptions {
  type: "none";
}

interface AudioContextOptions {
  latencyHint?: AudioContextLatencyCategory | number;
  /** Smudgy's physical session output currently requires 48,000 Hz. */
  sampleRate?: number;
  /** Omit or use "" for session output; "none" is hardware-independent. */
  sinkId?: "" | "none" | AudioSinkOptions;
}

interface GainOptions {
  gain?: number;
}

interface OscillatorOptions {
  detune?: number;
  frequency?: number;
  type?: OscillatorType;
}

interface AudioParam {
  /** Scalar mutation is supported; AudioParam automation methods are not. */
  value: number;
}

declare var AudioParam: {
  readonly prototype: AudioParam;
};

interface AudioNode extends EventTarget {
  readonly context: BaseAudioContext;
  readonly numberOfInputs: number;
  readonly numberOfOutputs: number;
  connect<T extends AudioNode>(destinationNode: T, output?: 0, input?: 0): T;
  connect(destinationParam: AudioParam, output?: 0): void;
  disconnect(): void;
  disconnect(output: 0): void;
  disconnect(destinationNode: AudioNode): void;
  disconnect(destinationNode: AudioNode, output: 0): void;
  disconnect(destinationNode: AudioNode, output: 0, input: 0): void;
  disconnect(destinationParam: AudioParam): void;
  disconnect(destinationParam: AudioParam, output: 0): void;
}

declare var AudioNode: {
  readonly prototype: AudioNode;
};

interface AudioDestinationNode extends AudioNode {
  readonly maxChannelCount: number;
}

declare var AudioDestinationNode: {
  readonly prototype: AudioDestinationNode;
};

interface BaseAudioContext extends EventTarget {
  readonly currentTime: number;
  readonly destination: AudioDestinationNode;
  onstatechange: ((this: BaseAudioContext, event: Event) => unknown) | null;
  readonly sampleRate: number;
  readonly state: AudioContextState;
  createGain(): GainNode;
  createOscillator(): OscillatorNode;
}

declare var BaseAudioContext: {
  readonly prototype: BaseAudioContext;
};

interface AudioContext extends BaseAudioContext {
  readonly baseLatency: number;
  readonly outputLatency: number;
  readonly sinkId: "" | "none";
  close(): Promise<void>;
  resume(): Promise<void>;
  suspend(): Promise<void>;
}

declare var AudioContext: {
  readonly prototype: AudioContext;
  new(contextOptions?: AudioContextOptions): AudioContext;
};

interface GainNode extends AudioNode {
  readonly gain: AudioParam;
}

declare var GainNode: {
  readonly prototype: GainNode;
  new(context: BaseAudioContext, options?: GainOptions): GainNode;
};

interface AudioScheduledSourceNode extends AudioNode {
  onended: ((this: AudioScheduledSourceNode, event: Event) => unknown) | null;
  start(when?: number): void;
  stop(when?: number): void;
}

declare var AudioScheduledSourceNode: {
  readonly prototype: AudioScheduledSourceNode;
};

interface OscillatorNode extends AudioScheduledSourceNode {
  readonly detune: AudioParam;
  readonly frequency: AudioParam;
  type: OscillatorType;
}

declare var OscillatorNode: {
  readonly prototype: OscillatorNode;
  new(context: BaseAudioContext, options?: OscillatorOptions): OscillatorNode;
};
