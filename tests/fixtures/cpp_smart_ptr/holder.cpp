#include "holder.h"

int Widget::Draw() { return 1; }

int WidgetBox::DrawHeld() { return held_->Draw(); }

int DrawThrough(Handle<Widget> handle) { return handle->Draw(); }
