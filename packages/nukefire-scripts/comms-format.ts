export interface AuctionPresentation {
  event: string;
  text: string;
}

export interface AuctionItemSpan {
  start: number;
  end: number;
  name: string;
}

const AUCTION_COMMAND = /^(?:auction|bid|endauction|whatsauc|aucstat|aucstatlive)(?:\s|$)/i;

/** Whether an input line is already one of the auction command family. */
export function isAuctionCommand(text: string): boolean {
  return AUCTION_COMMAND.test(text.trim());
}

/** Auctioneer system updates get the compact event presentation; players do not. */
export function isAuctioneer(player: string): boolean {
  return /^(?:the\s+)?auctioneer$/i.test(player.trim());
}

/** Locate item names in the streamlined auction text for Codex links. */
export function auctionItemSpans(text: string): AuctionItemSpan[] {
  const patterns = [
    /\bputs\s+(.+?)\s+up for auction\b/gi,
    /\b(?:Base item|Item):\s*(.+?)(?=\.(?:\s|$)|\s+(?:Bid with|To raise|Proxy max):|$)/gi,
    /\bcredits on\s+(.+?)(?=\.(?:\s|$)|$)/gi,
  ];
  const spans: AuctionItemSpan[] = [];

  for (const pattern of patterns) {
    let match: RegExpExecArray | null;
    while ((match = pattern.exec(text))) {
      const name = match[1].trim();
      const withinMatch = match[0].indexOf(match[1]);
      const leading = match[1].indexOf(name);
      const start = match.index + withinMatch + leading;
      const end = start + name.length;
      if (!spans.some((span) => start < span.end && end > span.start)) {
        spans.push({ start, end, name });
      }
    }
  }

  return spans.sort((a, b) => a.start - b.start);
}

/** Remove the Auctioneer's repeated speech wrapper and classify the update. */
export function auctionPresentation(rawMessage: string): AuctionPresentation {
  let text = rawMessage
    .replace(/^(?:the\s+)?Auctioneer auctions,\s*/i, "")
    .trim();

  const quoted = text.match(/^'(.*)'([.!?])?$/s);
  if (quoted) text = `${quoted[1]}${quoted[2] ?? ""}`;
  text = text.replace(/\s+/g, " ").replace(/\.!/g, ".");

  const events: ReadonlyArray<[RegExp, string]> = [
    [/^Auction notice:\s*/i, "NOTICE"],
    [/^Opening bid:\s*/i, "OPEN"],
    [/^Variant item\.\s*/i, "VARIANT"],
    [/^Requires\s+/i, "REQUIRES"],
    [/^Final call:\s*/i, "FINAL"],
    [/^Do I hear\s+/i, "ASK"],
    [/^Stat bonus:\s*/i, "BONUS"],
    [/^New bid:\s*/i, "BID"],
    [/^Going once:\s*/i, "ONCE"],
    [/^Going twice:\s*/i, "TWICE"],
    [/^Sold:\s*/i, "SOLD"],
  ];

  for (const [pattern, event] of events) {
    if (pattern.test(text)) {
      return {
        event,
        text: text.replace(pattern, "").replace(/\s*\|\s*/g, " · "),
      };
    }
  }

  return { event: "UPDATE", text: text.replace(/\s*\|\s*/g, " · ") };
}
