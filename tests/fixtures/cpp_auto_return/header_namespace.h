#pragma once
// `hdrns` is opened only here, never in the unit including this header.
namespace hdrns {
struct Job { void run(); virtual void go(); };
Job *make();
void body();
}
