// =============================================================================
//  Context Deck — the room's services as a compact vertical rail
// =============================================================================
//  NukeFire.Context describes what you can do *here* (remorter, packrat vault,
//  zone intelligence, …) as titled blocks with status lines and invocable
//  actions. Each block becomes a card: status tones color the values, actions
//  become buttons. An action with a `confirm` prompt raises a Modal before
//  sending; one with `arguments` is proposed into the command input instead
//  (Enter sends, typing amends), so the player supplies the arguments.

import { input, send, session } from "smudgy:core";
import {
  Button,
  Column,
  Container,
  Modal,
  Row,
  Scrollable,
  Space,
  Text,
  Tooltip,
  createWidget,
} from "smudgy:widgets";
import {
  nukefire,
  watchMessage,
  type NukeFireContextAction,
  type NukeFireContextEntry,
} from "smudgy://kapusniak/nukefire-gmcp";
import { widgetMetric, widgetTextSize } from "./config.ts";
import { visibleContexts } from "./context-model.ts";
import { UI, kindColor, themeBackground, toneColor } from "./theme.ts";

const PANE = "Deck";

let contexts: readonly NukeFireContextEntry[] = visibleContexts(
  nukefire.value?.NukeFire?.Context?.contexts ?? [],
);
let pendingConfirm: { action: NukeFireContextAction; from: string } | null = null;
let shown = false;

watchMessage("NukeFire.Context", (ctx) => {
  contexts = visibleContexts(ctx?.contexts ?? []);
  pendingConfirm = null; // room changed out from under a confirm prompt
  if (shown) mount();
});

function runAction(action: NukeFireContextAction, from: string): void {
  if (action.enabled === false) return;
  if (action.confirm) {
    pendingConfirm = { action, from };
    mount();
    return;
  }
  execute(action);
}

function execute(action: NukeFireContextAction): void {
  if (action.arguments && action.arguments.length > 0) {
    // Let the player fill the arguments: propose `command ` selected in the
    // input; Enter sends as-is, typing replaces.
    input.propose(`${action.command} `);
    input.focus();
    return;
  }
  send(action.command);
}

function actionContent(action: NukeFireContextAction, color: string, disabled = false) {
  return (
    <Row width="fill" spacing={6}>
      <Text size={widgetTextSize(10)} color={color}>{action.label}</Text>
      <Space width="fill" />
      {disabled
        ? <Text size={widgetTextSize(9)} color={UI.faint}>unavailable</Text>
        : action.arguments && action.arguments.length > 0
          ? <Text size={widgetTextSize(9)} color={UI.faint}>input …</Text>
          : null}
    </Row>
  );
}

function actionTooltip(action: NukeFireContextAction): string | undefined {
  if (action.enabled === false) return action.disabledReason || "unavailable";
  if (!action.arguments || action.arguments.length === 0) return action.help;

  const inputHint = "Continues in the command input so you can supply arguments.";
  return action.help ? `${action.help}\n${inputHint}` : inputHint;
}

function actionButton(entry: NukeFireContextEntry, action: NukeFireContextAction) {
  if (action.enabled === false) {
    return (
      <Tooltip tip={actionTooltip(action) ?? "unavailable"}>
        <Button width="fill" variant="subtle">
          {actionContent(action, UI.faint, true)}
        </Button>
      </Tooltip>
    );
  }
  const button = (
    <Button
      width="fill"
      variant="subtle"
      onPress={() => runAction(action, entry.title)}
    >
      {actionContent(action, UI.text)}
    </Button>
  );
  const tip = actionTooltip(action);
  return tip ? <Tooltip tip={tip}>{button}</Tooltip> : button;
}

function contextTitle(entry: NukeFireContextEntry) {
  const title = <Text size={widgetTextSize(12)} color={kindColor(entry.kind)}>{entry.title}</Text>;
  return entry.summary ? <Tooltip tip={entry.summary}>{title}</Tooltip> : title;
}

function card(entry: NukeFireContextEntry) {
  return (
    <Container width="fill">
      <Column width="fill" padding={10} spacing={6}>
        {[
          <Row width="fill" spacing={6}>
            {contextTitle(entry)}
            <Space width="fill" />
            <Text size={widgetTextSize(9)} color={UI.faint}>{entry.kind.toUpperCase()}</Text>
          </Row>,
          ...entry.status.map((s) => (
            <Row width="fill" spacing={6}>
              <Text size={widgetTextSize(10)} color={UI.dim}>{s.label}:</Text>
              <Container width="fill" align_x="right">
                <Text size={widgetTextSize(10)} color={toneColor(s.tone)}>{String(s.value)}</Text>
              </Container>
            </Row>
          )),
          <Column width="fill" spacing={4}>
            {entry.actions.map((action) => actionButton(entry, action))}
          </Column>,
        ]}
      </Column>
    </Container>
  );
}

function confirmModal() {
  const pending = pendingConfirm;
  if (!pending) return null;
  const dismiss = () => {
    pendingConfirm = null;
    mount();
  };
  return (
    <Modal onDismiss={dismiss}>
      <Container width="fill" background={themeBackground.bind()}>
        <Column width="fill" padding={14} spacing={10}>
          <Text size={widgetTextSize(12)} color={UI.header}>{pending.from}</Text>
          <Text size={widgetTextSize(12)} color={UI.text}>{pending.action.confirm}</Text>
          <Row spacing={8}>
            <Space width="fill" />
            <Button variant="subtle" onPress={dismiss}>
              <Text size={widgetTextSize(11)} color={UI.dim}>Cancel</Text>
            </Button>
            <Button
              variant="primary"
              onPress={() => {
                const action = pending.action;
                pendingConfirm = null;
                mount();
                execute(action);
              }}
            >
              <Text size={widgetTextSize(11)} color={pending.action.style === "danger" ? UI.danger : UI.bright}>
                {pending.action.label}
              </Text>
            </Button>
          </Row>
        </Column>
      </Container>
    </Modal>
  );
}

function mount(): void {
  createWidget(
    "nf-deck",
    <Column width="fill" height="fill" padding={4} spacing={4}>
      {[
        contexts.length === 0 ? (
          <Container width="fill" height="fill" align_x="center" align_y="center">
            <Text size={widgetTextSize(11)} color={UI.faint}>No services in this room.</Text>
          </Container>
        ) : (
          <Scrollable width="fill" height="fill">
            <Column width="fill" spacing={8}>{contexts.map(card)}</Column>
          </Scrollable>
        ),
        confirmModal(),
      ]}
    </Column>,
    { pane: PANE },
  );
}

export function open(): void {
  const parent = session.panes.get("Affects") ?? session.mainPane;
  parent.split("bottom", {
    name: PANE,
    height: widgetMetric(280),
    terminal: false,
  });
  shown = true;
  mount();
}

export function close(): void {
  shown = false;
  pendingConfirm = null;
  session.panes.get(PANE)?.close();
}
