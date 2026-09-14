#include "holder.h"

int Widget::Draw() { return 1; }

int WidgetBox::DrawHeld() { return held_->Draw(); }

int DrawThrough(Handle<Widget> handle) { return handle->Draw(); }

int DrawMissingField(MissingWidgetBox box) { return box.held->Draw(); }
int DrawNoArrow(HeaderNoArrow<Widget> box) { return box->Draw(); }
