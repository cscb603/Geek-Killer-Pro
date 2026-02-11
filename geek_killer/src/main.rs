#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use sysinfo::{System, Pid, Networks, Disks, ProcessRefreshKind};
use std::time::{Instant, Duration};
use eframe::egui;
use std::collections::HashMap;
use windows_sys::Win32::{
    Foundation::{CloseHandle, HANDLE},
    Security::{GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY},
    System::Threading::{GetCurrentProcess, OpenProcessToken},
};

fn is_admin() -> bool {
    unsafe {
        let mut token: HANDLE = 0;
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return false;
        }
        let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
        let mut return_length = 0;
        let success = GetTokenInformation(
            token,
            TokenElevation,
            &mut elevation as *mut _ as _,
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut return_length,
        ) != 0;
        CloseHandle(token);
        success && elevation.TokenIsElevated != 0
    }
}

fn setup_custom_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    
    // 尝试加载系统中的微软雅黑字体
    if let Ok(font_data) = std::fs::read("C:\\Windows\\Fonts\\msyh.ttc") {
        fonts.font_data.insert(
            "my_font".to_owned(),
            egui::FontData::from_owned(font_data),
        );
        
        fonts.families.get_mut(&egui::FontFamily::Proportional)
            .unwrap()
            .insert(0, "my_font".to_owned());
            
        fonts.families.get_mut(&egui::FontFamily::Monospace)
            .unwrap()
            .push("my_font".to_owned());
    }
    
    ctx.set_fonts(fonts);
}

struct GeekKillerApp {
    sys: System,
    networks: Networks,
    disks: Disks,
    search_query: String,
    last_refresh: Instant,
    is_admin: bool,
    show_performance: bool,
    show_diagnostics: bool,
    // 用于保持 UI 稳定的状态
    high_resource_cache: Vec<ProcessGroup>,
    other_groups_cache: Vec<ProcessGroup>,
    system_groups_cache: Vec<ProcessGroup>,
    // 复用缓冲区以减少分配
    groups_buffer: HashMap<String, ProcessGroup>,
}

#[derive(Clone)]
struct ProcessGroup {
    name: String,
    total_memory: u64,
    total_cpu: f32,
    pids: Vec<u32>,
    is_system: bool,
    is_not_responding: bool, // 新增：僵死状态检测
}

impl GeekKillerApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        setup_custom_fonts(&cc.egui_ctx);
        
        // 设置科技复古配色方案
        let mut visuals = egui::Visuals::dark();
        visuals.panel_fill = egui::Color32::from_rgb(20, 18, 15); // 深咖啡黑背景
        visuals.window_fill = egui::Color32::from_rgb(35, 30, 25);
        visuals.widgets.noninteractive.bg_fill = egui::Color32::from_rgb(30, 25, 20);
        visuals.widgets.inactive.bg_fill = egui::Color32::from_rgb(45, 35, 30);
        visuals.widgets.hovered.bg_fill = egui::Color32::from_rgb(60, 50, 40);
        visuals.widgets.active.bg_fill = egui::Color32::from_rgb(80, 65, 50);
        visuals.selection.bg_fill = egui::Color32::from_rgb(218, 165, 32); // 金色选中
        cc.egui_ctx.set_visuals(visuals);

        let mut sys = System::new_all();
        sys.refresh_all();
        let networks = Networks::new_with_refreshed_list();
        let disks = Disks::new_with_refreshed_list();
        
        let mut app = Self {
            sys,
            networks,
            disks,
            search_query: String::new(),
            last_refresh: Instant::now(),
            is_admin: is_admin(),
            show_performance: false,
            show_diagnostics: false,
            high_resource_cache: Vec::new(),
            other_groups_cache: Vec::new(),
            system_groups_cache: Vec::new(),
            groups_buffer: HashMap::with_capacity(512),
        };
        app.refresh_processes(true);
        app
    }

    fn refresh_processes(&mut self, force_resort: bool) {
        // 仅刷新必要信息，大幅降低 CPU 占用
        let refresh_kind = ProcessRefreshKind::new()
            .with_cpu()
            .with_memory();
        self.sys.refresh_processes_specifics(refresh_kind);
        
        self.groups_buffer.clear();

        for (pid, proc) in self.sys.processes() {
            let name = proc.name().to_string();
            let memory = proc.memory();
            let cpu = proc.cpu_usage();
            let pid_val = pid.as_u32();
            
            let status = proc.status();
            let is_unresponsive = matches!(status, sysinfo::ProcessStatus::UninterruptibleDiskSleep | sysinfo::ProcessStatus::Dead);

            let entry = self.groups_buffer.entry(name.clone()).or_insert(ProcessGroup {
                name,
                total_memory: 0,
                total_cpu: 0.0,
                pids: Vec::new(),
                is_system: pid_val < 1000,
                is_not_responding: false,
            });
            
            entry.total_memory += memory;
            entry.total_cpu += cpu;
            entry.pids.push(pid_val);
            if pid_val < 1000 {
                entry.is_system = true;
            }
            if is_unresponsive {
                entry.is_not_responding = true;
            }
        }

        // 只有在手动刷新或缓存为空时才重新分类和排序
        if force_resort || self.high_resource_cache.is_empty() {
            let mut group_list: Vec<ProcessGroup> = self.groups_buffer.values().cloned().collect();
            group_list.sort_by(|a, b| {
                b.total_memory.cmp(&a.total_memory)
                    .then_with(|| a.name.cmp(&b.name))
            });

            self.high_resource_cache.clear();
            self.other_groups_cache.clear();
            self.system_groups_cache.clear();

            for group in group_list {
                if group.total_cpu > 10.0 || group.total_memory > 500 * 1024 * 1024 {
                    self.high_resource_cache.push(group);
                } else if group.is_system {
                    self.system_groups_cache.push(group);
                } else {
                    self.other_groups_cache.push(group);
                }
            }
        } else {
            // 自动刷新模式：仅更新现有缓存中的数据值
            let update_vec = |cache: &mut Vec<ProcessGroup>, all_groups: &HashMap<String, ProcessGroup>| {
                for item in cache.iter_mut() {
                    if let Some(new_data) = all_groups.get(&item.name) {
                        item.total_memory = new_data.total_memory;
                        item.total_cpu = new_data.total_cpu;
                        item.pids = new_data.pids.clone();
                        item.is_not_responding = new_data.is_not_responding;
                    } else {
                        item.total_memory = 0;
                        item.total_cpu = 0.0;
                        item.pids.clear();
                    }
                }
                cache.retain(|g| !g.pids.is_empty());
            };

            let buf = &self.groups_buffer;
            update_vec(&mut self.high_resource_cache, buf);
            update_vec(&mut self.other_groups_cache, buf);
            update_vec(&mut self.system_groups_cache, buf);
        }
    }

    fn render_process_table(&mut self, ui: &mut egui::Ui, groups: Vec<ProcessGroup>, is_high: bool) {
        let text_color = egui::Color32::from_rgb(218, 165, 32); // 复古金
        
        // 动态计算列宽，确保在小窗口下也能自适应
        let available_width = ui.available_width() - 40.0; // 减去间距
        let name_col_width = (available_width - 320.0).max(150.0); // 动态分配名称列

        egui::Grid::new(format!("grid_{}", if is_high { "high" } else if groups.first().is_some_and(|g| g.is_system) { "system" } else { "other" }))
            .num_columns(5)
            .spacing([15.0, 10.0])
            .striped(true)
            .show(ui, |ui| {
                ui.add_sized([40.0, 20.0], egui::Label::new(egui::RichText::new("数量").strong().color(text_color)));
                ui.add_sized([name_col_width, 20.0], egui::Label::new(egui::RichText::new("进程名称").strong().color(text_color)));
                ui.add_sized([90.0, 20.0], egui::Label::new(egui::RichText::new("总内存").strong().color(text_color)));
                ui.add_sized([70.0, 20.0], egui::Label::new(egui::RichText::new("总CPU").strong().color(text_color)));
                ui.add_sized([80.0, 20.0], egui::Label::new(egui::RichText::new("操作").strong().color(text_color)));
                ui.end_row();

                for group in groups {
                    // 数量
                    ui.add_sized([40.0, 20.0], egui::Label::new(egui::RichText::new(format!("x{}", group.pids.len())).monospace()));
                    
                    // 名称 (自适应截断)
                    ui.add_sized([name_col_width, 20.0], |ui: &mut egui::Ui| {
                        ui.horizontal(|ui| {
                            let name_color = if is_high { egui::Color32::from_rgb(255, 140, 0) } else { egui::Color32::from_rgb(200, 180, 150) };
                            let label = egui::Label::new(egui::RichText::new(&group.name).color(name_color).strong())
                                .truncate(true);
                            ui.add(label);
                            
                            if group.is_system {
                                ui.label(egui::RichText::new("SYS").small().color(egui::Color32::from_rgb(139, 115, 85)));
                            }
                            if group.is_not_responding {
                                ui.label(egui::RichText::new("DEAD").small().color(egui::Color32::RED));
                            }
                        }).response
                    });

                    // 内存
                    ui.add_sized([90.0, 20.0], egui::Label::new(format!("{:.1} MB", group.total_memory as f32 / 1024.0 / 1024.0)));
                    
                    // CPU
                    let cpu_color = if group.total_cpu > 20.0 { egui::Color32::RED } else if group.total_cpu > 5.0 { egui::Color32::GOLD } else { egui::Color32::from_rgb(150, 255, 150) };
                    ui.add_sized([70.0, 20.0], egui::Label::new(egui::RichText::new(format!("{:.1}%", group.total_cpu)).color(cpu_color).monospace()));

                    // 操作
                    ui.add_sized([80.0, 20.0], |ui: &mut egui::Ui| {
                        let btn = egui::Button::new(egui::RichText::new("终止").color(egui::Color32::WHITE))
                            .fill(egui::Color32::from_rgb(180, 40, 40))
                            .rounding(4.0);
                        let res = ui.add(btn);
                        if res.clicked() {
                            for pid in &group.pids {
                                if let Some(p) = self.sys.process(Pid::from(*pid as usize)) {
                                    p.kill();
                                }
                            }
                        }
                        res
                    });
                    ui.end_row();
                }
            });
    }
}

impl eframe::App for GeekKillerApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // 降低刷新频率：每秒只刷新一次，大幅降低 CPU 占用
        let elapsed = self.last_refresh.elapsed();
        if elapsed > Duration::from_secs(1) {
            self.refresh_processes(false);
            self.sys.refresh_cpu_usage();
            self.sys.refresh_memory();
            self.networks.refresh();
            self.disks.refresh();
            self.last_refresh = Instant::now();
        }

        // 关键：动态计算下次重绘时间，确保刷新频率稳定在 1s
        let time_until_next = Duration::from_secs(1).saturating_sub(self.last_refresh.elapsed());
        ctx.request_repaint_after(time_until_next);

        egui::CentralPanel::default().show(ctx, |ui| {
            // 整体内边距
            ui.spacing_mut().item_spacing = egui::vec2(8.0, 12.0);

            // Header with Retro Style
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.heading(egui::RichText::new("GEEK KILLER PRO").strong().color(egui::Color32::from_rgb(218, 165, 32)));
                    ui.label(egui::RichText::new("星TAP实验室 出品 | cscb603@qq.com").small().color(egui::Color32::from_rgb(100, 80, 60)));
                });
                
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if self.is_admin {
                        ui.label(egui::RichText::new("ADMIN MODE").color(egui::Color32::from_rgb(0, 255, 127)).strong());
                    } else {
                        ui.label(egui::RichText::new("USER MODE").color(egui::Color32::GOLD).strong());
                    }
                });
            });

            ui.add_space(15.0); // 增加顶部留白

            // Controls with Tech Style
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("扫描器:").color(egui::Color32::from_rgb(150, 130, 100)));
                ui.add(egui::TextEdit::singleline(&mut self.search_query)
                    .hint_text("搜索进程名...")
                    .desired_width(180.0));
                
                ui.add_space(10.0);
                
                if ui.add(egui::Button::new("立即刷新")
                    .min_size(egui::vec2(90.0, 26.0))
                    .rounding(4.0)
                    .stroke(egui::Stroke::new(1.0, egui::Color32::from_rgb(100, 85, 70))))
                    .clicked() {
                    self.refresh_processes(true);
                }

                ui.toggle_value(&mut self.show_performance, "性能监测");
                ui.toggle_value(&mut self.show_diagnostics, "智能诊断");
            });

            ui.add_space(20.0); // 增加搜索栏与列表的间距

            // 智能诊断面板
            if self.show_diagnostics {
                ui.allocate_ui(egui::vec2(ui.available_width(), 120.0), |ui| {
                    egui::Frame::group(ui.style())
                        .fill(egui::Color32::from_rgb(30, 25, 20))
                        .stroke(egui::Stroke::new(1.0, egui::Color32::from_rgb(218, 165, 32)))
                        .rounding(4.0)
                        .show(ui, |ui| {
                            ui.set_min_height(100.0);
                            ui.vertical(|ui| {
                                ui.label(egui::RichText::new("智能诊断报告").strong().color(egui::Color32::from_rgb(218, 165, 32)));
                                ui.add_space(5.0);
                                
                                let mut issues = Vec::new();
                                // 从缓存中诊断，避免跳动
                                for group in self.high_resource_cache.iter().chain(self.other_groups_cache.iter()) {
                                    if group.is_not_responding {
                                        issues.push(format!("检测到僵死进程: {} (建议立即终止)", group.name));
                                    }
                                    if group.total_cpu > 80.0 {
                                        issues.push(format!("严重 CPU 占用: {} ({:.1}%)", group.name, group.total_cpu));
                                    }
                                    if group.total_memory > 2 * 1024 * 1024 * 1024 {
                                        issues.push(format!("大量内存占用: {} ({:.1} GB)", group.name, group.total_memory as f32 / 1024.0 / 1024.0 / 1024.0));
                                    }
                                }

                                if issues.is_empty() {
                                    ui.label(egui::RichText::new("未发现明显异常进程").color(egui::Color32::GREEN));
                                } else {
                                    egui::ScrollArea::vertical()
                                        .id_source("diag_scroll")
                                        .max_height(80.0)
                                        .show(ui, |ui| {
                                            for issue in issues {
                                                ui.label(egui::RichText::new(format!("- {}", issue)).color(egui::Color32::from_rgb(255, 100, 100)));
                                            }
                                        });
                                }
                            });
                        });
                });
                ui.add_space(15.0);
            }

            // 性能监测面板
            if self.show_performance {
                ui.allocate_ui(egui::vec2(ui.available_width(), 160.0), |ui| {
                    egui::Frame::group(ui.style())
                        .fill(egui::Color32::from_rgb(25, 22, 18))
                        .stroke(egui::Stroke::new(1.0, egui::Color32::from_rgb(60, 50, 40)))
                        .rounding(4.0)
                        .show(ui, |ui| {
                            ui.set_min_height(140.0);
                            ui.horizontal(|ui| {
                                ui.label(egui::RichText::new("系统遥测面板").strong().color(egui::Color32::from_rgb(218, 165, 32)));
                            });
                            
                            egui::Grid::new("perf_grid").num_columns(2).spacing([20.0, 8.0]).show(ui, |ui| {
                                let cpu_usage = self.sys.global_cpu_info().cpu_usage();
                                ui.label("中央处理器 (CPU):");
                                ui.add(egui::ProgressBar::new(cpu_usage / 100.0)
                                    .text(format!("{:.1}%", cpu_usage))
                                    .fill(egui::Color32::from_rgb(218, 165, 32)));
                                ui.end_row();

                                let total_mem = self.sys.total_memory();
                                let used_mem = self.sys.used_memory();
                                let mem_usage = used_mem as f32 / total_mem as f32;
                                ui.label("物理内存 (RAM):");
                                ui.add(egui::ProgressBar::new(mem_usage)
                                    .text(format!("{:.1}GB / {:.1}GB", used_mem as f32 / 1024.0/1024.0/1024.0, total_mem as f32 / 1024.0/1024.0/1024.0))
                                    .fill(egui::Color32::from_rgb(180, 150, 100)));
                                ui.end_row();

                                let mut net_in = 0;
                                let mut net_out = 0;
                                for (_, data) in self.networks.iter() {
                                    net_in += data.received();
                                    net_out += data.transmitted();
                                }
                                ui.label("网络流量 (NET):");
                                ui.label(format!("In: {:.1} KB/s | Out: {:.1} KB/s", net_in as f32 / 1024.0, net_out as f32 / 1024.0));
                                ui.end_row();

                                ui.label("磁盘存储 (DISK):");
                                if let Some(disk) = self.disks.iter().next() {
                                    let free = disk.available_space();
                                    let total = disk.total_space();
                                    ui.label(format!("{:.1}GB 可用 / {:.1}GB 总计", free as f32 / 1024.0/1024.0/1024.0, total as f32 / 1024.0/1024.0/1024.0));
                                }
                                ui.end_row();
                            });
                        });
                });
                ui.add_space(15.0);
            }

            // Process List with Categorization
            ui.push_id("process_list", |ui| {
                ui.spacing_mut().item_spacing.y = 15.0; // 增加各组之间的间距

                // 1. 极高负载任务 (核心关注区)
                let high_resource = self.high_resource_cache.iter()
                    .filter(|g| self.search_query.is_empty() || g.name.to_lowercase().contains(&self.search_query.to_lowercase()))
                    .cloned()
                    .collect::<Vec<_>>();

                if !high_resource.is_empty() {
                    ui.group(|ui| {
                        ui.horizontal(|ui| {
                            ui.label(egui::RichText::new("🔥 极高负载任务").color(egui::Color32::from_rgb(255, 69, 0)).strong());
                            ui.label(egui::RichText::new(format!("({})", high_resource.len())).small().color(egui::Color32::GRAY));
                        });
                        ui.add_space(5.0);
                        egui::ScrollArea::vertical()
                            .id_source("high_scroll")
                            .max_height(250.0) // 限制高度，防止占满全屏
                            .auto_shrink([false, true])
                            .show(ui, |ui| {
                                self.render_process_table(ui, high_resource, true);
                            });
                    });
                }

                ui.add_space(5.0);

                // 2. 活动用户任务
                let other_groups = self.other_groups_cache.iter()
                    .filter(|g| self.search_query.is_empty() || g.name.to_lowercase().contains(&self.search_query.to_lowercase()))
                    .cloned()
                    .collect::<Vec<_>>();

                ui.group(|ui| {
                    egui::CollapsingHeader::new(egui::RichText::new(format!("👤 活动用户任务 ({})", other_groups.len()))
                        .color(egui::Color32::from_rgb(100, 149, 237)).strong())
                        .default_open(true)
                        .show(ui, |ui| {
                            ui.add_space(5.0);
                            egui::ScrollArea::vertical()
                                .id_source("other_scroll")
                                .max_height(300.0)
                                .auto_shrink([false, true])
                                .show(ui, |ui| {
                                    self.render_process_table(ui, other_groups, false);
                                });
                        });
                });

                ui.add_space(5.0);

                // 3. 系统核心服务
                let system_groups = self.system_groups_cache.iter()
                    .filter(|g| self.search_query.is_empty() || g.name.to_lowercase().contains(&self.search_query.to_lowercase()))
                    .cloned()
                    .collect::<Vec<_>>();

                ui.group(|ui| {
                    egui::CollapsingHeader::new(egui::RichText::new(format!("🛡️ 系统核心服务 ({})", system_groups.len()))
                        .color(egui::Color32::from_rgb(139, 115, 85)).strong())
                        .default_open(false)
                        .show(ui, |ui| {
                            ui.add_space(5.0);
                            egui::ScrollArea::vertical()
                                .id_source("system_scroll")
                                .max_height(200.0)
                                .auto_shrink([false, true])
                                .show(ui, |ui| {
                                    self.render_process_table(ui, system_groups, false);
                                });
                        });
                });
            });

            // 底部增加留白，避免内容紧贴边缘
            ui.add_space(20.0);
        });
    }
}

fn main() -> eframe::Result<()> {
    // 采用嵌入方式加载图标，确保在任何环境下都能显示，不依赖外部文件
    let icon_data = include_bytes!("../../进程图标.png");
    let icon = image::load_from_memory(icon_data)
        .ok()
        .map(|img| {
            let rgba = img.to_rgba8();
            let (width, height) = rgba.dimensions();
            egui::IconData {
                rgba: rgba.into_raw(),
                width,
                height,
            }
        });

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([650.0, 800.0])
            .with_min_inner_size([600.0, 400.0])
            .with_icon(icon.unwrap_or_default()),
        ..Default::default()
    };
    
    eframe::run_native(
        "Geek Killer Pro",
        native_options,
        Box::new(|cc| Box::new(GeekKillerApp::new(cc))),
    )
}
