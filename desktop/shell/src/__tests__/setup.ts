/// jsdom has no layout, so it has no `scrollIntoView`. Chromium does, and the
/// app only ever runs in Chromium — shimmed here rather than guarded in the
/// product, because a guard in the product would be dead code written for a
/// browser this app never runs in.
Element.prototype.scrollIntoView = function scrollIntoView() {};
