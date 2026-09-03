use anyhow::Result;
use ratatui::{
    buffer::Buffer,
    crossterm::event::{Event, KeyCode},
    layout::{Constraint, Layout, Rect},
    style::Stylize,
    text::{Line, Text},
    widgets::{Block, Borders, Padding, StatefulWidget, Widget},
};
use tokio::sync::mpsc::UnboundedSender;

use crate::worker::{Request, Response};

#[derive(Debug)]
pub struct RawSession {
    software_id: u16,
    tx: UnboundedSender<Request>,
    status: String,
    data_label: Option<&'static str>,
    data: Vec<u8>,
}

impl RawSession {
    pub fn create(software_id: u16, tx: UnboundedSender<Request>) -> Self {
        Self {
            software_id,
            tx,
            status: "Unknown device connected; read-only diagnostic mode".into(),
            data_label: None,
            data: Vec::new(),
        }
    }

    pub fn handle_event(&mut self, event: &Event) -> Result<bool> {
        let Some(key) = event.as_key_press_event() else {
            return Ok(false);
        };
        let request = match key.code {
            KeyCode::Char('i') => Some(Request::RawQuerySoftwareId),
            KeyCode::Char('u') => Some(Request::RawUnlockRead { key: 0x0000 }),
            KeyCode::Char('m') => Some(Request::RawReadMemory16 {
                key: 0x0000,
                address: 0x0000,
            }),
            KeyCode::Char('e') => Some(Request::RawReadEeprom16 {
                key: 0x0000,
                address: 0x0000,
            }),
            _ => None,
        };
        if let Some(request) = request {
            self.status = "Running diagnostic request...".into();
            self.tx.send(request)?;
            return Ok(true);
        }
        Ok(false)
    }

    pub fn handle_worker_response(&mut self, response: Response) {
        match response {
            Response::RawStatus(status) => self.status = status,
            Response::RawData { label, data } => {
                self.status = format!("{label}: {} bytes received", data.len());
                self.data_label = Some(label);
                self.data = data;
            }
            _ => {}
        }
    }

    fn render_data(&self) -> Text<'static> {
        if self.data.is_empty() {
            return Text::from("No raw data captured yet.");
        }
        let mut lines = Vec::new();
        if let Some(label) = self.data_label {
            lines.push(Line::from(label.to_string()).bold());
        }
        for (row, chunk) in self.data.chunks(16).enumerate() {
            let mut line = format!("{:04x}: ", row * 16);
            for byte in chunk {
                line.push_str(&format!("{byte:02x} "));
            }
            lines.push(Line::from(line));
        }
        Text::from(lines)
    }
}

impl StatefulWidget for &RawSession {
    type State = Option<ratatui::layout::Position>;
    fn render(self, area: Rect, buf: &mut Buffer, _state: &mut Self::State) {
        let outer = Block::bordered()
            .borders(Borders::ALL)
            .padding(Padding::proportional(1))
            .title(
                Line::from(format!(
                    " Unknown Miele device — Software ID {} / 0x{:04x} ",
                    self.software_id, self.software_id
                ))
                .bold(),
            );
        let inner = outer.inner(area);
        outer.render(area, buf);
        let [info, status, data] = Layout::vertical([
            Constraint::Length(5),
            Constraint::Length(3),
            Constraint::Fill(1),
        ])
        .spacing(1)
        .areas(inner);
        Text::from(vec![
            Line::from("Read-only diagnostic controls").bold(),
            Line::from("i  query software ID"),
            Line::from("u  unlock read access with key 0x0000"),
            Line::from("m  read 16 bytes memory at address 0x0000"),
            Line::from("e  read 16 bytes EEPROM at word address 0x0000"),
        ])
        .render(info, buf);
        let sb = Block::bordered().title(" Status ");
        let si = sb.inner(status);
        sb.render(status, buf);
        self.status.as_str().render(si, buf);
        let db = Block::bordered().title(" Raw data ");
        let di = db.inner(data);
        db.render(data, buf);
        self.render_data().render(di, buf);
    }
}
