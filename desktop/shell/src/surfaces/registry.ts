import type { ComponentType } from "react";
import type { Desk } from "../desk";

/// What the shell needs to know to host a surface, and nothing more.
///
/// Mail, a board or a browser join by calling `register`. The shell reads the
/// list; it never imports a surface by name. Keeping this contract small is
/// what stops the shell from growing a special case per feature.
export type Surface = {
  id: string;
  /// The word shown in the rail. Not an icon: a rail of glyphs stops being
  /// readable at about six entries, and this one is meant to reach a dozen.
  label: string;
  group: "work" | "setup";
  /// Live count beside the label — how many things here want attention.
  /// Returns undefined when a count would be noise rather than information,
  /// which includes "the runtime is not connected": a badge over placeholder
  /// data is a number that means nothing.
  badge?: (desk: Desk) => number | undefined;
  view: ComponentType;
  /// Rendered above the content. Chat deliberately has none: a toolbar over a
  /// conversation is chrome with nothing to say.
  toolbar?: ComponentType;
  /// Shown in the drawer when it is summoned on this surface.
  drawer?: ComponentType;
  /// The input row, for surfaces where typing is the action. Declared here
  /// rather than imported by the shell so that a mail surface or a board can
  /// bring its own without the shell learning about either.
  composer?: ComponentType;
  /// What the status line says while this surface is showing. A surface that
  /// declares none leaves the row to the shell.
  status?: ComponentType;
  /// Commands contributed to the palette. Declared rather than registered
  /// imperatively, so the palette can list everything available without every
  /// surface having been mounted first.
  commands?: { id: string; title: string; hint?: string }[];
};

const surfaces: Surface[] = [];

export function register(surface: Surface): void {
  surfaces.push(surface);
}

export function all(): readonly Surface[] {
  return surfaces;
}

export function byId(id: string): Surface | undefined {
  return surfaces.find((surface) => surface.id === id);
}

/// Every command every surface declares, for the palette.
export function commands(): { surface: string; id: string; title: string; hint?: string }[] {
  return surfaces.flatMap((surface) =>
    (surface.commands ?? []).map((command) => ({ surface: surface.id, ...command })),
  );
}
