use super::*;

use myuri_core::config::SpeechProvider;
use speech::TtsOptions;

pub(super) struct SpeechInputs {
    stability: Entity<InputState>,
    similarity: Entity<InputState>,
    style: Entity<InputState>,
    speed: Entity<InputState>,
    language: Entity<InputState>,
    output_format: Entity<InputState>,
    fetching_models: bool,
}

impl SpeechInputs {
    pub(super) fn new(window: &mut Window, cx: &mut Context<SettingsView>) -> Self {
        let mut input = |hint| cx.new(|cx| InputState::new(window, cx).placeholder(hint));
        Self {
            stability: input("0 到 1"),
            similarity: input("0 到 1"),
            style: input("0 到 1"),
            speed: input("0.25 到 2"),
            language: input("auto"),
            output_format: input("pcm_24000"),
            fetching_models: false,
        }
    }

    pub(super) fn fill(
        &mut self,
        provider: &SpeechProvider,
        window: &mut Window,
        cx: &mut Context<SettingsView>,
    ) {
        for (input, value) in [
            (&self.stability, provider.stability),
            (&self.similarity, provider.similarity_boost),
            (&self.style, provider.style),
            (&self.speed, provider.speed),
        ] {
            let value = value.map(|value| value.to_string()).unwrap_or_default();
            input.update(cx, |input, cx| input.set_value(value, window, cx));
        }
        for (input, value) in [
            (&self.language, &provider.language),
            (&self.output_format, &provider.output_format),
        ] {
            input.update(cx, |input, cx| input.set_value(value.clone(), window, cx));
        }
        self.fetching_models = false;
    }

    pub(super) fn options(&self, api: SpeechApi, cx: &App) -> Result<TtsOptions, String> {
        let read = |input: &Entity<InputState>| input.read(cx).value().trim().to_string();
        let number = |input: &Entity<InputState>, label: &str, min: f32, max: f32| {
            let value = read(input);
            if value.is_empty() {
                return Ok(None);
            }
            value
                .parse::<f32>()
                .ok()
                .filter(|number| (min..=max).contains(number))
                .map(Some)
                .ok_or_else(|| format!("{label}需在 {min} 到 {max} 之间"))
        };
        let (speed_min, speed_max) = match api {
            SpeechApi::Elevenlabs => (0.7, 1.2),
            SpeechApi::Soniox => (0.7, 1.3),
            SpeechApi::Openai => (0.25, 2.0),
        };
        let eleven = api == SpeechApi::Elevenlabs;
        let output_format = if eleven {
            let raw = read(&self.output_format);
            if raw.is_empty() {
                String::new()
            } else {
                myuri_core::speech::validate_elevenlabs_format(&raw)?
            }
        } else {
            String::new()
        };
        Ok(TtsOptions {
            voice: String::new(),
            stability: if eleven { number(&self.stability, "稳定度", 0.0, 1.0)? } else { None },
            similarity_boost: if eleven { number(&self.similarity, "相似度", 0.0, 1.0)? } else { None },
            style: if eleven { number(&self.style, "表现力", 0.0, 1.0)? } else { None },
            speed: number(&self.speed, "语速", speed_min, speed_max)?,
            language: if api != SpeechApi::Openai { read(&self.language) } else { String::new() },
            output_format,
        })
    }
}

#[derive(Clone, Copy)]
enum SpeechAction {
    Voices,
    Preview,
    Models,
}

enum SpeechResult {
    Voices(Vec<Voice>),
    Preview(speech::Clip),
    Models(Vec<String>, Vec<String>),
}

impl SettingsView {
    pub(super) fn speech_fields(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let palette = self.palette;
        let eleven = self.speech_api == SpeechApi::Elevenlabs;
        let apis = [
            (SpeechApi::Elevenlabs, "ElevenLabs"),
            (SpeechApi::Soniox, "Soniox"),
            (SpeechApi::Openai, "OpenAI"),
        ]
        .into_iter()
        .enumerate()
        .map(|(index, (api, label))| {
            choice(
                ("speech-api", index),
                label,
                api == self.speech_api,
                palette,
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                if this.speech_api != api {
                    this.speech_api = api;
                    this.form_generation = this.form_generation.wrapping_add(1);
                    this.voice_options.clear();
                    this.voice_fetching = false;
                    this.testing = false;
                    this.speech_inputs.fetching_models = false;
                    cx.notify();
                }
            }))
        });
        let voice_buttons = div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap(px(8.0))
            .when(eleven, |row| {
                row.child(
                    button(
                        "fetch-voices",
                        if self.voice_fetching {
                            "拉取中…"
                        } else {
                            "拉取音色"
                        },
                        false,
                        palette,
                    )
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.speech_action(SpeechAction::Voices, window, cx)
                    })),
                )
            })
            .child(
                button(
                    "preview-voice",
                    if self.testing {
                        "合成中…"
                    } else {
                        "试听"
                    },
                    false,
                    palette,
                )
                .on_click(cx.listener(|this, _, window, cx| {
                    this.speech_action(SpeechAction::Preview, window, cx)
                })),
            );
        let voice = self.voice.read(cx).value();
        let voice_options = div()
            .id("voice-options")
            .max_h(px(192.0))
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap(px(2.0))
            .children(
                self.voice_options
                    .iter()
                    .enumerate()
                    .map(|(index, option)| {
                        let label = if option.name.is_empty() {
                            &option.id
                        } else {
                            &option.name
                        };
                        let id = option.id.clone();
                        sheet_row(("voice", index), palette, option.id == voice)
                            .child(
                                div()
                                    .flex_1()
                                    .min_w(px(0.0))
                                    .truncate()
                                    .child(label.clone()),
                            )
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.voice.update(cx, |input, cx| {
                                    input.set_value(id.clone(), window, cx)
                                });
                                cx.notify();
                            }))
                    }),
            );
        let mut fields = vec![
            field(
                "语音接口",
                div().flex().flex_wrap().gap(px(6.0)).children(apis),
                palette,
            ),
            field(
                "语音识别模型",
                boxed(Textarea::new(&self.asr_models).appearance(false), palette),
                palette,
            ),
            field(
                "语音合成模型",
                boxed(Textarea::new(&self.tts_models).appearance(false), palette),
                palette,
            ),
        ];
        if self.speech_api != SpeechApi::Soniox {
            fields.push(
                div()
                    .child(
                        button(
                            "speech-models",
                            if self.speech_inputs.fetching_models {
                                "拉取中…"
                            } else {
                                "拉取语音模型"
                            },
                            false,
                            palette,
                        )
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.speech_action(SpeechAction::Models, window, cx)
                        })),
                    )
                    .into_any_element(),
            );
        }
        fields.push(field(
            "音色",
            div()
                .flex()
                .flex_col()
                .gap(px(6.0))
                .child(boxed(Input::new(&self.voice).appearance(false), palette))
                .child(voice_buttons)
                .when(eleven && !self.voice_options.is_empty(), |body| {
                    body.child(voice_options)
                }),
            palette,
        ));

        let inputs = &self.speech_inputs;
        if eleven {
            fields.extend([
                number_field(
                    "稳定度",
                    &inputs.stability,
                    &[
                        ("创意 0.3", "0.3"),
                        ("自然 0.5", "0.5"),
                        ("稳定 0.8", "0.8"),
                    ],
                    palette,
                    cx,
                ),
                number_field("相似度", &inputs.similarity, &[], palette, cx),
                number_field("表现力", &inputs.style, &[], palette, cx),
            ]);
        }
        let speed_presets: &[(&str, &str)] = if eleven {
            &[
                ("0.85×", "0.85"),
                ("1.0×", "1.0"),
                ("1.1×", "1.1"),
                ("1.2×", "1.2"),
            ]
        } else {
            &[
                ("0.85×", "0.85"),
                ("1.0×", "1.0"),
                ("1.15×", "1.15"),
                ("1.3×", "1.3"),
            ]
        };
        fields.push(number_field("语速", &inputs.speed, speed_presets, palette, cx));
        if self.speech_api != SpeechApi::Openai {
            fields.push(field(
                "语种",
                boxed(Input::new(&inputs.language).appearance(false), palette),
                palette,
            ));
        }
        if eleven {
            fields.push(field(
                "输出格式",
                boxed(Input::new(&inputs.output_format).appearance(false), palette),
                palette,
            ));
        }
        fields
    }

    fn speech_action(&mut self, action: SpeechAction, window: &Window, cx: &mut Context<Self>) {
        if self.speech_pending(action) {
            return;
        }
        let (provider, key) = match self.speech_draft(cx) {
            Ok(draft) => draft,
            Err(reason) => {
                self.status = Some((false, reason));
                return cx.notify();
            }
        };
        self.set_speech_pending(action, true);
        self.status = None;
        let generation = self.form_generation;
        let base_url = provider.base_url.clone();
        let (tx, rx) = async_channel::bounded(1);
        std::thread::spawn(move || {
            let key = key.as_deref();
            let result = match action {
                SpeechAction::Voices => {
                    speech::voices_draft(&provider, key).map(SpeechResult::Voices)
                }
                SpeechAction::Models => {
                    speech::models(&provider, key).map(|(asr, tts)| SpeechResult::Models(asr, tts))
                }
                SpeechAction::Preview => {
                    let options = TtsOptions {
                        output_format: "pcm_24000".into(),
                        ..Default::default()
                    };
                    provider
                        .tts_models
                        .first()
                        .ok_or_else(|| "还没有语音合成模型".to_string())
                        .and_then(|model| {
                            speech::tts_draft(&provider, model, "你好，我是缪里。", &options, key)
                        })
                        .map(SpeechResult::Preview)
                }
            };
            let _ = tx.send_blocking(result);
        });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(result) = rx.recv().await else { return };
            let _ = this.update_in(cx, |this, window, cx| {
                if this.form_generation != generation || this.tab != Tab::Speech {
                    return;
                }
                this.set_speech_pending(action, false);
                if this.draft_url(cx) != base_url {
                    return cx.notify();
                }
                match result {
                    Ok(SpeechResult::Voices(voices)) => {
                        if voices.is_empty() {
                            this.status = Some((true, "这条渠道没有返回音色".into()));
                        }
                        this.voice_options = voices;
                    }
                    Ok(SpeechResult::Preview(clip)) => {
                        if let Err(reason) = speech::play(clip.bytes) {
                            this.status = Some((false, reason));
                        }
                    }
                    Ok(SpeechResult::Models(asr, tts)) => {
                        let added = append_models(&this.asr_models, asr, window, cx)
                            + append_models(&this.tts_models, tts, window, cx);
                        this.status = Some((
                            true,
                            if added == 0 {
                                "没有新语音模型".into()
                            } else {
                                format!("已加入 {added} 个语音模型")
                            },
                        ));
                    }
                    Err(reason) => this.status = Some((false, reason)),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn speech_pending(&self, action: SpeechAction) -> bool {
        match action {
            SpeechAction::Voices => self.voice_fetching,
            SpeechAction::Preview => self.testing,
            SpeechAction::Models => self.speech_inputs.fetching_models,
        }
    }

    fn set_speech_pending(&mut self, action: SpeechAction, pending: bool) {
        match action {
            SpeechAction::Voices => self.voice_fetching = pending,
            SpeechAction::Preview => self.testing = pending,
            SpeechAction::Models => self.speech_inputs.fetching_models = pending,
        }
    }
}

fn append_models(
    input: &Entity<TextareaState>,
    models: Vec<String>,
    window: &mut Window,
    cx: &mut Context<SettingsView>,
) -> usize {
    let mut current = lines_of(input, cx);
    let old = current.len();
    for model in models {
        if !current.contains(&model) {
            current.push(model);
        }
    }
    let added = current.len() - old;
    if added > 0 {
        input.update(cx, |input, cx| {
            input.set_value(current.join("\n"), window, cx)
        });
    }
    added
}

fn number_field(
    label: &'static str,
    input: &Entity<InputState>,
    presets: &[(&'static str, &'static str)],
    palette: Palette,
    cx: &mut Context<SettingsView>,
) -> AnyElement {
    let current = input.read(cx).value().trim().parse::<f32>().ok();
    let choices = presets
        .iter()
        .copied()
        .enumerate()
        .map(|(index, (caption, value))| {
            let active = current == value.parse().ok();
            let input = input.clone();
            choice((label, index), caption, active, palette).on_click(cx.listener(
                move |_, _, window, cx| {
                    input.update(cx, |input, cx| input.set_value(value, window, cx));
                    cx.notify();
                },
            ))
        });
    field(
        label,
        div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap(px(8.0))
            .children(choices)
            .child(boxed(Input::new(input).appearance(false), palette).w(px(90.0))),
        palette,
    )
}
