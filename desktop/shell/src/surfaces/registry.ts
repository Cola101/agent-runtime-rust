import type { ComponentType } from "react";
import type { Desk } from "../desk";

/// A key a surface claims while it is showing.
///
/// Declared as data rather than bound with an event listener inside a view, so
/// the shell can render a key hint from the same source that dispatches it.
/// A hint drawn from one place and a binding registered in another is how a
/// screen ends up promising keys that do nothing.
export type KeyBinding = {
  /// `KeyboardEvent.key`, matched verbatim.
  key: string;
  /// Shown next to the action it performs. The shell renders these; a surface
  /// never writes a key hint by hand.
  hint: string;
  run(desk: Desk): void;
  /// False when the binding is not currently meaningful — nothing selected,
  /// nothing waiting. A hint for an inert key is worse than no hint.
  when?(desk: Desk): boolean;
};

export type Command = {
  id: string;
  title: string;
  hint?: string;
  /// What the command does. A command that only navigates says so by omitting
  /// this; anything else has to actually do something, because a palette whose
  /// entries all just switch tabs is a tab bar with extra steps.
  run?(desk: Desk): void;
  when?(desk: Desk): boolean;
};

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
  /// Shown in the drawer when it is summoned on this surface. A surface that
  /// declares none makes the drawer key inert, and the shell says so rather
  /// than offering a key that opens nothing.
  drawer?: ComponentType;
  /// The label for the drawer, so the shell can name what ⌘I would open.
  drawerLabel?: string;
  /// The input row, for surfaces where typing is the action. Declared here
  /// rather than imported by the shell so that a mail surface or a board can
  /// bring its own without the shell learning about either.
  composer?: ComponentType;
  /// What the status line says while this surface is showing. A surface that
  /// declares none leaves the row to the shell.
  status?: ComponentType;
  /// Keys this surface claims while it is showing.
  keys?: KeyBinding[];
  /// Commands contributed to the palette. Declared rather than registered
  /// imperatively, so the palette can list everything available without every
  /// surface having been mounted first.
  commands?: Command[];
};

const surfaces: Surface[] = [];

/// Registering an id twice replaces it rather than adding a second entry.
///
/// Two surfaces with one id is never what anyone meant. It happens during
/// development, where a hot reload re-runs a module that already registered --
/// the rail grows a second 对话 and both render. Replacing keeps the newly
/// loaded definition, which is the one the reload was for.
export function register(surface: Surface): void {
  const existing = surfaces.findIndex((candidate) => candidate.id === surface.id);
  if (existing === -1) surfaces.push(surface);
  else surfaces[existing] = surface;
}

export function all(): readonly Surface[] {
  return surfaces;
}

export function byId(id: string): Surface | undefined {
  return surfaces.find((surface) => surface.id === id);
}

export type ResolvedCommand = Command & { surface: string; surfaceLabel: string };

/// Every command every surface declares, for the palette.
export function commands(): ResolvedCommand[] {
  return surfaces.flatMap((surface) =>
    (surface.commands ?? []).map((command) => ({
      ...command,
      surface: surface.id,
      surfaceLabel: surface.label,
    })),
  );
}
