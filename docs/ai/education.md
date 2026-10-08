# Education: "teach me fractions"

Status: direction (2026-10-08), not started. The user's favourite and the hardest case of
[`software-on-demand.md`](software-on-demand.md): the generated app must also model the
learner. Parent principles: [`../ux/principles.md`](../ux/principles.md).

## What the evidence says about LLM tutors

- **Unguarded access harms learning.** Bastani et al. (PNAS 2025, ~1,000 high-school maths
  students): ChatGPT-style GPT-4 raised practice scores 48% and **lowered exam scores 17%**;
  a guarded tutor (hints, teacher input) raised practice 127% and left the exam **equal** to
  control.
- **Guarded tutors get abandoned.** Khanmigo, two-year RCT in 18 Tennessee schools (NBER):
  +0.06-0.08 SD/year, the same as Khan Academy without AI; students used it in 17% of sessions
  with a mistake, tried to extract answers, and stopped when it asked questions instead.
  The binding constraint is engagement.
- **Good results need design and adults.** Harvard physics (Kestin, Sci. Reports 2025): a
  course-specific tutor doubled gains on first exposure. Nigeria (World Bank 2024): 0.23-0.3 SD
  in six weeks, teacher-supported. OLPC Peru (IDB 2012): laptops alone, no gain.
- **Calibrate:** Bloom's 2σ never replicated; in education > 0.20 SD is large (Kraft).

The trap: an LLM that answers harms; one that refuses gets abandoned. Every rule below exits it.

## Domain principles

- **E1. The LLM authors the material; it is not the interlocutor while solving.** It builds a
  microworld (Papert), diagnoses and adjusts between sessions. Nothing in the solving loop to
  extract answers from. (Google's "Learning Interactives" generates candidate simulations for
  a teacher to pick, on the same premise.)
- **E2. Desirable difficulty, even when it feels worse** (Bjork). Spacing, interleaving,
  retrieval; learners rate effortful strategies as less effective (Kirk-Johnson 2019), so
  "liked it" and practice scores are the wrong targets.
- **E3. Guidance adapts to prior knowledge.** Novices get worked examples (Kirschner, Sweller &
  Clark 2006); learners with a base get the problem first (Kapur's productive failure,
  weaker in primary school). The learner model decides.
- **E4. An adult stays in the loop.** Progress is readable by a parent or teacher in 30 s.

Cases of the parent principles:
- **P1 (explains itself):** the material shows the error by itself, Montessori's *control of
  error* (e.g. 1/12 placed right of 1/2 on a number line is visibly the shorter bar).
- **P1.3 / P3.2:** the learner model is a structured, local, readable file.
- **P7.3:** the learner can open the microworld and change it (Kay, Resnick's high ceiling).
- **P8.3:** success is delayed, unaided recall; never practice scores, minutes or streaks.
- **P9:** spaced review arrives in the periphery at a calm moment; no streaks (Duolingo burnout).
- **P6.1:** a child's account lacks capabilities instead of having bypassable locks.

## Techniques with evidence

- **Fractions:** the whole-number bias (Siegler: "1/12 > 1/2 because 12 > 2", 3/5 + 5/6 = 8/11);
  number-line instruction beat area models in Fuchs' RCTs (WWC: meets standards without
  reservations; the package had several components).
- **Diagnosis from actions, not text:** LLMs diagnose misconceptions ~84% when constrained by
  topic, but F1 < 0.5 with false confidence on open diagnosis (MathCog). Use structured
  microworld actions via the semantic tree (B5) and knowledge tracing (BKT) per skill.
- **Teach the agent (protégé effect):** students worked harder for a teachable agent than for
  themselves, most for low achievers (Betty's Brain; Chase et al. 2009). The LLM plays a
  student with the target misconception; there is no answer to extract.
- **Embedded spaced prompts:** Quantum Country's mnemonic medium (~6 s per prompt, review days
  later). Designers' account; no independent evaluation found.
- **Wide walls for free:** the same objective themed on the learner's interest (football,
  music) costs the LLM nothing extra.

## The flow

1. "teach me fractions" → 2 minutes of probe tasks in a microworld; diagnosis from actions.
2. Learner model written (local file): skills mastered, misconceptions suspected.
3. Microworld generated on the learner's interest, with control of error; three.js runtime.
4. Worked examples or problem-first, per E3.
5. "Teach Pip" mode: an agent that believes 1/12 > 1/2.
6. Days later, two 6-second review prompts in the periphery.
7. "Want to see how it's built?" opens the source.
8. Weekly summary for the adult.

## Why the OS and not an app

Khanmigo can't: time reviews without interrupting (needs B6 + work spheres), see meaningful
actions in any microworld (B5), run model-written code safely (B3), let the child open the code
(P7.3), keep the learner model local (P3.2).

## Caveats

- A minor's data: local by default; anything sent to an API is visible and minimal.
- Most technique studies are small; measure our own effect with delayed unaided tests (P8.3).
